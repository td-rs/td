//! `CommunityManager` (td/telegram/CommunityManager.{h,cpp}) — the M.34
//! Wave-3 community manager: the community cache plane and the two request
//! flows (createCommunity/setCommunityName — the only community td_api
//! arms in this revision). Recorded collapses: the Community/CommunityFull
//! byte-exact store/parse + the sqlite-pmc persistence join when the
//! Photo/DialogPhoto store/parse land on master (they live on the unmerged
//! M.9/M.10 lines); the photo object legs answer `None`; the
//! create/rename/permissions/delete queries ride the client-side
//! dispatcher under their `CommunityNetKind` (the answer routing lives in
//! `Td::on_community_net_answer`; the create answer chains behind
//! `on_get_updates` into `finish_create_community`); the full
//! dialog-access plane is the env seam; the dispatch-dependency argument
//! of the C++ `net_query_creator().create` calls collapses (the port's
//! NetQuery carries no dependency set).

use crate::chat_manager::{AdministratorRights, DialogParticipantStatus, RestrictedRights};
use crate::community_id::CommunityId;
use crate::dialog_id::DialogId;
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait CommunityManagerEnv {
    fn close_flag(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `DialogManager::check_dialog_access` — the create gate.
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status>;

    /// `DialogManager::get_chat_id_object` — the collapsed sync form.
    fn get_chat_id_object(&mut self, dialog_id: DialogId) -> i64;

    /// `CreateCommunityQuery::send` (CommunityManager.cpp:129-136) —
    /// `communities.create` over the read-access input peer of the
    /// dialog; the answer routes through `on_get_updates` with the
    /// `finish_create_community` chain. The dispatch-dependency argument
    /// of the C++ `net_query_creator().create` collapses (the port's
    /// NetQuery carries no dependency set).
    fn send_create_community_query(&mut self, name: &str, dialog_id: DialogId, is_hidden: bool) {
        tdutils::log_info!(
            "Send communities.create(name = {name}, is_hidden = {is_hidden}) for {dialog_id:?} — [net-inert]"
        );
        let _ = dialog_id;
    }

    /// `GetFullCommunityQuery::send` — the community-full cache load
    /// (no ported flow drives it yet; the seam stays for the
    /// get-community plane).
    fn send_get_full_community_query(&mut self, community_id: CommunityId) {
        tdutils::log_info!("Send channels.getFullCommunity for {community_id:?} — [net-inert]");
    }

    /// `EditCommunityTitleQuery::send` (CommunityManager.cpp:163-194) —
    /// `channels.editTitle` over `get_input_community`; the resolved
    /// input channel rides the seam (the manager is taken out under the
    /// env).
    fn send_set_community_name_query(
        &mut self,
        input_community: telegram_api::InputChannel,
        name: &str,
    ) {
        tdutils::log_info!("Send channels.editTitle to {name:?} — [net-inert]");
        let _ = input_community;
    }

    /// Whether the AuthorizationManager reports a bot — the
    /// `get_input_peer`/`get_input_community` bots-tolerant arm
    /// (CommunityManager.cpp:1375-1398).
    fn auth_is_bot(&self) -> bool {
        false
    }

    /// `EditCommunityDefaultBannedRightsQuery::send` (d55f0a694,
    /// CommunityManager.cpp:197-229) — `messages_editChatDefaultBannedRights`
    /// over `get_input_peer` (the resolved peer rides the seam).
    fn send_set_community_permissions_query(
        &mut self,
        input_peer: telegram_api::InputPeer,
        permissions: &RestrictedRights,
    ) {
        tdutils::log_info!(
            "Send messages.editChatDefaultBannedRights, rights = {:?} — [net-inert]",
            permissions.get_chat_banned_rights()
        );
        let _ = input_peer;
    }

    /// `DeleteCommunityQuery::send` (f5f7fe5fd, CommunityManager.cpp:231-260)
    /// — `channels.deleteChannel` over `get_input_community`.
    fn send_delete_community_query(&mut self, input_community: telegram_api::InputChannel) {
        tdutils::log_info!("Send channels.deleteChannel — [net-inert]");
        let _ = input_community;
    }
}

/// `CommunityManager::Community` — the cached community record. The
/// byte-exact store/parse is recorded above.
#[derive(Debug, Default)]
pub struct Community {
    pub access_hash: i64,
    pub title: String,
    pub status: DialogParticipantStatus,
    pub date: i32,
    pub collapsed_in_dialogs: bool,
    pub is_changed: bool,
}

/// `CommunityManager::CommunityDialog` — a linked chat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommunityDialog {
    dialog_id: DialogId,
    can_view_history: bool,
    is_visible: bool,
}

impl CommunityDialog {
    /// The wire ctor (CommunityManager.cpp:257-260).
    pub fn from_telegram(peer: &telegram_api::CommunityPeer) -> CommunityDialog {
        let dialog_id = peer
            .peer
            .as_ref()
            .map_or(DialogId::default(), DialogId::from_peer);
        CommunityDialog {
            dialog_id,
            can_view_history: peer.can_view_history,
            is_visible: peer.visible,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.dialog_id.is_valid()
    }

    /// `get_community_chat_object` (CommunityManager.cpp:265-269).
    pub fn get_community_chat_object(
        &self,
        env: &mut impl CommunityManagerEnv,
    ) -> td_api::CommunityChat {
        td_api::CommunityChat {
            chat_id: env.get_chat_id_object(self.dialog_id),
            can_view_history: self.can_view_history,
            is_hidden: !self.is_visible,
        }
    }
}

/// `CommunityManager::CommunityFull` — the full record (the store/parse
/// is recorded above).
#[derive(Debug, Default, PartialEq)]
pub struct CommunityFull {
    pub about: String,
    pub dialogs: Vec<CommunityDialog>,
    pub administrator_count: i32,
    pub banned_count: i32,
    pub peer_link_requests_pending: i32,
}

/// The collapsed `CommunityManager`.
#[derive(Default)]
pub struct CommunityManager {
    communities: std::collections::BTreeMap<i64, Community>,
    communities_full: std::collections::BTreeMap<i64, CommunityFull>,
    /// The `td_api::CommunityId`-typed promises parked at the create
    /// seam (the answer chains behind `on_get_updates` into
    /// `finish_create_community`).
    pub parked_create_promises: Vec<Promise<td_api::CommunityId>>,
    /// The `()`-typed promises parked at the rename seam (the
    /// `channels.editTitle` answer family).
    pub pending_rename: Vec<Promise<()>>,
    /// The `EditCommunityDefaultBannedRightsQuery` promise lot.
    pub pending_permissions: Vec<Promise<()>>,
    /// The `DeleteCommunityQuery` promise lot.
    pub pending_delete: Vec<Promise<()>>,
}

impl CommunityManager {
    /// Pop the head promise of a family lot — the answer-routing half of
    /// a query family (the C++ handler's own promise).
    pub(crate) fn take_promise<T: std::marker::Send>(
        &mut self,
        slot: for<'a> fn(&'a mut CommunityManager) -> &'a mut Vec<Promise<T>>,
    ) -> Option<Promise<T>> {
        let lot = slot(self);
        if lot.is_empty() {
            None
        } else {
            Some(lot.remove(0))
        }
    }

    pub fn new() -> CommunityManager {
        CommunityManager::default()
    }

    /// `have_community` (CommunityManager.cpp).
    pub fn have_community(&self, community_id: CommunityId) -> bool {
        self.communities.contains_key(&community_id.get())
    }

    fn add_community(&mut self, community_id: CommunityId, source: &str) -> &mut Community {
        let _ = source;
        self.communities
            .entry(community_id.get())
            .or_insert_with(|| Community {
                is_changed: true,
                ..Community::default()
            })
    }

    fn get_community(&self, community_id: CommunityId) -> Option<&Community> {
        self.communities.get(&community_id.get())
    }

    fn add_community_full(&mut self, community_id: CommunityId) -> &mut CommunityFull {
        self.communities_full.entry(community_id.get()).or_default()
    }

    /// `on_get_community` (CommunityManager.cpp:728-810) — the wire
    /// intake (the photo + default-permission planes are the recorded
    /// seams).
    pub fn on_get_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community: &telegram_api::community,
        source: &str,
    ) {
        let community_id = CommunityId::new(community.id);
        if !community_id.is_valid() {
            tdutils::log_error!("Receive invalid {community_id:?} from {source}");
            return;
        }
        if community.flags == 0 && community.access_hash == 0 && community.title.is_empty() {
            return;
        }
        let is_min = community.min;
        let access_hash = community.access_hash;
        if access_hash == 0 && !is_min {
            tdutils::log_error!(
                "Receive non-min {community_id:?} without access_hash from {source}"
            );
            return;
        }
        let status = if community.creator {
            DialogParticipantStatus::creator(!community.left, false, String::new())
        } else if let Some(admin_rights) = &community.admin_rights {
            DialogParticipantStatus::administrator(
                AdministratorRights::from_chat_admin_rights(
                    Some(admin_rights),
                    crate::chat_manager::ChannelType::Unknown,
                ),
                String::new(),
                false,
            )
        } else if community.left {
            DialogParticipantStatus::left()
        } else {
            DialogParticipantStatus::member(0, String::new())
        };

        let community_record = self.add_community(community_id, source);
        if community_record.access_hash != access_hash
            && (!is_min || community_record.access_hash == 0)
        {
            community_record.access_hash = access_hash;
            community_record.is_changed = true;
        }
        if community_record.date != community.date && (!is_min || community_record.date == 0) {
            community_record.date = community.date;
            community_record.is_changed = true;
        }
        if community_record.collapsed_in_dialogs != community.collapsed_in_dialogs && !is_min {
            community_record.collapsed_in_dialogs = community.collapsed_in_dialogs;
            community_record.is_changed = true;
        }
        if !is_min {
            if community_record.status != status {
                community_record.status = status;
                community_record.is_changed = true;
            }
            if community_record.title != community.title {
                community_record.title = community.title.clone();
                community_record.is_changed = true;
            }
        }
        if community_record.is_changed {
            community_record.is_changed = false;
            env.send_update(self.get_update_community_object(community_id));
        }
    }

    /// `on_get_community_forbidden` (CommunityManager.cpp).
    pub fn on_get_community_forbidden(&mut self, community_id: CommunityId, source: &str) {
        let _ = source;
        self.communities.remove(&community_id.get());
        self.communities_full.remove(&community_id.get());
    }

    /// `on_get_community_full` (CommunityManager.cpp:1010-1054).
    pub fn on_get_community_full(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community: &telegram_api::communityFull,
    ) {
        let community_id = CommunityId::new(community.id);
        if self.get_community(community_id).is_none() {
            tdutils::log_error!("Can't find {community_id:?}");
            return;
        }
        let community_full = self.add_community_full(community_id);
        let community_dialogs: Vec<CommunityDialog> = community
            .linked_peers
            .iter()
            .map(CommunityDialog::from_telegram)
            .filter(|dialog| dialog.is_valid())
            .collect();
        let is_changed = community_full.about != community.about
            || community_full.dialogs != community_dialogs
            || community_full.administrator_count != community.admins_count
            || community_full.banned_count != community.kicked_count
            || community_full.peer_link_requests_pending != community.peer_link_requests_pending;
        if is_changed {
            community_full.about = community.about.clone();
            community_full.dialogs = community_dialogs;
            community_full.administrator_count = community.admins_count;
            community_full.banned_count = community.kicked_count;
            community_full.peer_link_requests_pending = community.peer_link_requests_pending;
        }
        if is_changed {
            let update = self.get_update_community_full_info_object(env, community_id);
            env.send_update(update);
        }
    }

    /// `create_community` (CommunityManager.cpp:1079-1084).
    pub fn create_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        name: &str,
        dialog_id: DialogId,
        is_hidden: bool,
        mut promise: Promise<td_api::CommunityId>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        env.send_create_community_query(name, dialog_id, is_hidden);
        self.parked_create_promises.push(promise);
    }

    /// `finish_create_community` (CommunityManager.cpp:1086-1091).
    pub fn finish_create_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut promise: Promise<td_api::CommunityId>,
    ) {
        if env.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        promise.set_value(td_api::CommunityId {
            id: community_id.get(),
        });
    }

    /// `set_community_name` (CommunityManager.cpp:1093-1107).
    pub fn set_community_name(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        name: &str,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.is_administrator() || !community.status.can_change_info_and_settings()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change community name",
            ));
        }
        // `EditCommunityTitleQuery::send` resolves `get_input_community`
        // (the C++ CHECK band is the cached-community invariant above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_set_community_name_query(input_community, name);
        self.pending_rename.push(promise);
    }

    /// `set_community_photo` gates (fb05f53c2,
    /// CommunityManager.cpp:1170-1183) — the rights half of the manager
    /// method; the `DialogManager::do_set_dialog_photo` ladder over
    /// `community_id.get_fake_dialog_id()` runs from the request arm (the
    /// port's Td-level photo plane).
    pub fn set_community_photo_gates(
        &self,
        community_id: CommunityId,
    ) -> std::result::Result<(), Status> {
        let Some(community) = self.get_community(community_id) else {
            return Err(Status::error(400, "Community not found"));
        };
        if !community.status.is_administrator() || !community.status.can_change_info_and_settings()
        {
            return Err(Status::error(400, "Have not enough rights"));
        }
        Ok(())
    }

    /// `set_community_permissions` (d55f0a694,
    /// CommunityManager.cpp:1219-1236).
    pub fn set_community_permissions(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        permissions: Option<&td_api::CommunityPermissions>,
        mut promise: Promise<()>,
    ) {
        let Some(permissions) = permissions else {
            return promise.set_error(Status::error(400, "New permissions must be non-empty"));
        };
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_restrict_members() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `EditCommunityDefaultBannedRightsQuery::send` resolves the
        // community's input peer.
        let Some(input_peer) = self.get_input_peer(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_set_community_permissions_query(
            input_peer,
            &RestrictedRights::from_community_permissions(Some(permissions)),
        );
        self.pending_permissions.push(promise);
    }

    /// `delete_community` (f5f7fe5fd, CommunityManager.cpp:1268-1278).
    pub fn delete_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.is_creator() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `DeleteCommunityQuery::send` resolves `get_input_community`.
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_delete_community_query(input_community);
        self.pending_delete.push(promise);
    }

    /// `get_input_community` (1118a50df, CommunityManager.cpp:1373-1385).
    pub fn get_input_community(
        &self,
        env: &impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<telegram_api::InputChannel> {
        let mut access_hash = 0;
        match self.get_community(community_id) {
            Some(community) => access_hash = community.access_hash,
            None => {
                if !env.auth_is_bot() || !community_id.is_valid() {
                    return None;
                }
            }
        }
        Some(telegram_api::InputChannel::inputChannel(
            telegram_api::inputChannel {
                channel_id: community_id.get(),
                access_hash,
            },
        ))
    }

    /// `get_input_peer` (1118a50df, CommunityManager.cpp:1387-1398).
    pub fn get_input_peer(
        &self,
        env: &impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<telegram_api::InputPeer> {
        let mut access_hash = 0;
        match self.get_community(community_id) {
            Some(community) => access_hash = community.access_hash,
            None => {
                if !env.auth_is_bot() || !community_id.is_valid() {
                    return None;
                }
            }
        }
        Some(telegram_api::InputPeer::inputPeerChannel(
            telegram_api::inputPeerChannel {
                channel_id: community_id.get(),
                access_hash,
            },
        ))
    }

    /// `get_community_object` (CommunityManager.cpp) — the td_api form
    /// (the photo leg answers `None` — recorded).
    pub fn get_community_object(&self, community_id: CommunityId) -> Option<td_api::Community> {
        let community = self.get_community(community_id)?;
        Some(td_api::Community {
            id: community_id.get(),
            have_access: !community.status.is_left(),
            name: community.title.clone(),
            photo: None,
            date: community.date,
            status: Some(Self::get_community_member_status_object(&community.status)),
            permissions: Some(td_api::CommunityPermissions {
                can_edit_chat_list: !community.collapsed_in_dialogs,
            }),
        })
    }

    /// The wire-status → td_api mapping (the C++ derives it from the
    /// participant status).
    fn get_community_member_status_object(
        status: &DialogParticipantStatus,
    ) -> td_api::CommunityMemberStatus {
        if status.is_creator() {
            td_api::CommunityMemberStatus::communityMemberStatusCreator(
                td_api::communityMemberStatusCreator,
            )
        } else if status.is_administrator() {
            td_api::CommunityMemberStatus::communityMemberStatusAdministrator(
                td_api::communityMemberStatusAdministrator {
                    can_be_edited: status.can_be_edited(),
                    rights: None,
                },
            )
        } else if status.is_left() {
            td_api::CommunityMemberStatus::communityMemberStatusLeft(
                td_api::communityMemberStatusLeft,
            )
        } else {
            td_api::CommunityMemberStatus::communityMemberStatusMember(
                td_api::communityMemberStatusMember,
            )
        }
    }

    /// `get_update_community_object` (CommunityManager.cpp).
    pub fn get_update_community_object(&self, community_id: CommunityId) -> td_api::Update {
        td_api::Update::updateCommunity(td_api::updateCommunity {
            community: self.get_community_object(community_id),
        })
    }

    /// `get_community_full_info_object` (CommunityManager.cpp).
    pub fn get_community_full_info_object(
        &self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<td_api::CommunityFullInfo> {
        let community_full = self.communities_full.get(&community_id.get())?;
        Some(td_api::CommunityFullInfo {
            photo: None,
            chats: community_full
                .dialogs
                .iter()
                .map(|dialog| dialog.get_community_chat_object(env))
                .collect(),
            administrator_count: community_full.administrator_count,
            banned_count: community_full.banned_count,
            add_chat_request_count: community_full.peer_link_requests_pending,
        })
    }

    /// `get_update_community_full_info_object` (CommunityManager.cpp).
    pub fn get_update_community_full_info_object(
        &self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> td_api::Update {
        td_api::Update::updateCommunityFullInfo(td_api::updateCommunityFullInfo {
            community_id: community_id.get(),
            community_full_info: self.get_community_full_info_object(env, community_id),
        })
    }

    /// `get_current_state` (Requests.cpp:2205 area).
    pub fn get_current_state(&self, updates: &mut Vec<td_api::Update>) {
        for community_id in self.communities.keys() {
            updates.push(self.get_update_community_object(CommunityId::new(*community_id)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl CommunityManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn check_dialog_access(&mut self, _dialog_id: DialogId) -> std::result::Result<(), Status> {
            Ok(())
        }

        fn get_chat_id_object(&mut self, _dialog_id: DialogId) -> i64 {
            3
        }

        fn send_create_community_query(
            &mut self,
            name: &str,
            _dialog_id: DialogId,
            is_hidden: bool,
        ) {
            self.queries
                .push(format!("createCommunity {name} {is_hidden}"));
        }

        fn send_set_community_name_query(
            &mut self,
            input_community: telegram_api::InputChannel,
            name: &str,
        ) {
            let channel_id = match &input_community {
                telegram_api::InputChannel::inputChannel(channel) => channel.channel_id,
                _ => 0,
            };
            self.queries
                .push(format!("setCommunityName {channel_id} {name}"));
        }

        fn send_set_community_permissions_query(
            &mut self,
            input_peer: telegram_api::InputPeer,
            _permissions: &RestrictedRights,
        ) {
            let community_id = match &input_peer {
                telegram_api::InputPeer::inputPeerChannel(peer) => peer.channel_id,
                _ => 0,
            };
            self.queries
                .push(format!("setCommunityPermissions {community_id}"));
        }

        fn send_delete_community_query(&mut self, input_community: telegram_api::InputChannel) {
            let channel_id = match &input_community {
                telegram_api::InputChannel::inputChannel(channel) => channel.channel_id,
                _ => 0,
            };
            self.queries.push(format!("deleteCommunity {channel_id}"));
        }
    }

    fn wire_community(id: i64, title: &str) -> telegram_api::community {
        telegram_api::community {
            flags: 1,
            creator: true,
            left: false,
            min: false,
            flags2: 0,
            collapsed_in_dialogs: false,
            id,
            access_hash: 5,
            title: title.to_string(),
            photo: None,
            date: 10,
            admin_rights: None,
            default_banned_rights: None,
        }
    }

    fn wire_peer() -> telegram_api::CommunityPeer {
        telegram_api::CommunityPeer {
            flags: 0,
            can_view_history: true,
            visible: true,
            peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                channel_id: 3,
            })),
        }
    }

    #[test]
    fn on_get_community_caches_and_emits() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        assert!(manager.have_community(CommunityId::new(7)));
        assert_eq!(env.updates.len(), 1);

        let object = manager.get_community_object(CommunityId::new(7)).unwrap();
        assert_eq!(object.name, "My community");
        assert!(object.have_access);
        assert!(matches!(
            object.status,
            Some(td_api::CommunityMemberStatus::communityMemberStatusCreator(
                _
            ))
        ));
    }

    #[test]
    fn forbidden_drops_the_record() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        manager.on_get_community_forbidden(CommunityId::new(7), "test");
        assert!(!manager.have_community(CommunityId::new(7)));
    }

    #[test]
    fn create_parks_and_finish_settles() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        let (promise, recorded) = record_id();
        manager.create_community(&mut env, "name", DialogId::new(1), true, promise);
        assert!(env.queries.iter().any(|q| q == "createCommunity name true"));
        assert_eq!(manager.parked_create_promises.len(), 1);
        let _ = recorded;

        let (promise, recorded) = record_id();
        manager.finish_create_community(&mut env, CommunityId::new(9), promise);
        assert_eq!(recorded.0.lock().unwrap().take().unwrap().id, 9);
    }

    #[test]
    fn rename_requires_admin_rights() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        let (promise, recorded) = record();
        manager.set_community_name(&mut env, CommunityId::new(7), "new name", promise);
        assert!(
            env.queries
                .iter()
                .any(|q| q == "setCommunityName 7 new name")
        );
        assert_eq!(manager.pending_rename.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the rename promise parks"
        );

        let (promise, recorded) = record();
        manager.set_community_name(&mut env, CommunityId::new(8), "new name", promise);
        assert!(recorded.0.lock().unwrap().is_some());
    }

    #[test]
    fn full_info_builds_the_chat_list() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        manager.on_get_community_full(
            &mut env,
            &telegram_api::communityFull {
                flags: 0,
                id: 7,
                about: "about".to_string(),
                chat_photo: None,
                linked_peers: vec![wire_peer()],
                admins_count: 2,
                kicked_count: 1,
                peer_link_requests_pending: 0,
            },
        );
        let info = manager.get_community_full_info_object(&mut env, CommunityId::new(7));
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.administrator_count, 2);
        assert_eq!(info.banned_count, 1);
        assert_eq!(info.chats.len(), 1);
        assert_eq!(info.chats[0].chat_id, 3);
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    struct Recording(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl tdutils::promise::PromiseInterface<()> for Recording {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record() -> (Promise<()>, Recorded) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            Recorded(recorded),
        )
    }

    struct RecordedId(std::sync::Arc<std::sync::Mutex<Option<td_api::CommunityId>>>);

    struct RecordingId(std::sync::Arc<std::sync::Mutex<Option<td_api::CommunityId>>>);

    impl tdutils::promise::PromiseInterface<td_api::CommunityId> for RecordingId {
        fn set_value(&mut self, value: td_api::CommunityId) {
            *self.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn record_id() -> (Promise<td_api::CommunityId>, RecordedId) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingId(recorded.clone())),
            RecordedId(recorded),
        )
    }

    /// A non-creator member wire community (on_get_community derives the
    /// member status when creator/left/admin_rights are all unset).
    fn wire_member_community(id: i64) -> telegram_api::community {
        telegram_api::community {
            creator: false,
            left: false,
            admin_rights: None,
            ..wire_community(id, "My community")
        }
    }

    #[test]
    fn photo_gates_mirror_the_manager() {
        // CommunityManager.cpp:1170-1183 (fb05f53c2): not found → 400,
        // a non-admin member → "Have not enough rights".
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        assert!(
            manager
                .set_community_photo_gates(CommunityId::new(7))
                .is_ok()
        );
        assert_eq!(
            manager
                .set_community_photo_gates(CommunityId::new(8))
                .unwrap_err()
                .message(),
            "Community not found"
        );

        manager.on_get_community(&mut env, &wire_member_community(9), "test");
        assert_eq!(
            manager
                .set_community_photo_gates(CommunityId::new(9))
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
    }

    #[test]
    fn permissions_and_delete_gates() {
        // CommunityManager.cpp:1219-1236 (d55f0a694) and 1268-1278
        // (f5f7fe5fd): the null-permissions band, the restrict/creator
        // gates, and the parked query sends.
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        let (promise, recorded) = record();
        manager.set_community_permissions(&mut env, CommunityId::new(7), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "New permissions must be non-empty"
        );

        let (promise, recorded) = record();
        manager.set_community_permissions(&mut env, CommunityId::new(8), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "New permissions must be non-empty"
        );

        let (promise, recorded) = record();
        manager.set_community_permissions(
            &mut env,
            CommunityId::new(8),
            Some(&td_api::CommunityPermissions {
                can_edit_chat_list: true,
            }),
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        let (promise, _recorded) = record();
        manager.set_community_permissions(
            &mut env,
            CommunityId::new(7),
            Some(&td_api::CommunityPermissions {
                can_edit_chat_list: true,
            }),
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q.starts_with("setCommunityPermissions 7"))
        );
        assert_eq!(manager.pending_permissions.len(), 1);

        let (promise, recorded) = record();
        manager.delete_community(&mut env, CommunityId::new(7), promise);
        assert!(env.queries.iter().any(|q| q == "deleteCommunity 7"));
        assert_eq!(manager.pending_delete.len(), 1);
        assert!(!recorded.0.lock().unwrap().is_some(), "the promise parks");

        manager.on_get_community(&mut env, &wire_member_community(9), "test");
        let (promise, recorded) = record();
        manager.delete_community(&mut env, CommunityId::new(9), promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
    }

    #[test]
    fn input_peer_builders() {
        // CommunityManager.cpp:1373-1398 (1118a50df): the cached access
        // hash rides, unknown communities answer None unless a bot asks
        // for a valid id.
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        let input_channel = manager
            .get_input_community(&env, CommunityId::new(7))
            .unwrap();
        match input_channel {
            telegram_api::InputChannel::inputChannel(c) => {
                assert_eq!(c.channel_id, 7);
                assert_eq!(c.access_hash, 5);
            }
            other => panic!("unexpected input channel {other:?}"),
        }
        let input_peer = manager.get_input_peer(&env, CommunityId::new(7)).unwrap();
        match input_peer {
            telegram_api::InputPeer::inputPeerChannel(p) => {
                assert_eq!(p.channel_id, 7);
                assert_eq!(p.access_hash, 5);
            }
            other => panic!("unexpected input peer {other:?}"),
        }
        assert!(
            manager
                .get_input_community(&env, CommunityId::new(8))
                .is_none()
        );
        assert!(manager.get_input_peer(&env, CommunityId::new(0)).is_none());
    }
}
