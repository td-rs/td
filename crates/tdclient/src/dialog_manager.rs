//! Port of `td/telegram/DialogManager.{h,cpp}` — the synchronous facade
//! region (M.6.15–M.6.19): input peers and access checks
//! (DialogManager.cpp:1350-1535), the dialog-info reload/force-create
//! dispatch (:1536-1630 + :1633-1682), the td_api builders and predicates
//! (:1690-1752 + header + :1942-2028), the recent-dialog lists + hints
//! (:1811-1915, RecentDialogList.cpp), the delete/migrate/report/blocked
//! surface (:1755-1809 + :2030-2340 + :2800-2960), the server-sync
//! binlog mirrors and toggles (:3491-3831), the username resolve family
//! (:3018-3325) and the dialog edit surface (:2341-2799).
//!
//! The C++ DialogManager is an actor; the state that survives this slice
//! ([`DialogManager`]) is held by `Td`, and the per-call surface is
//! module-level functions over `&mut Td` — the same single-threaded access
//! pattern as the rest of the port.
//!
//! # M.6 stub inventory (every `td_->…->` call site this slice cannot reach
//! yet — flip these when the owning slice lands)
//!
//! | C++ call site | stand-in here |
//! |---|---|
//! | `messages_manager_->have_dialog_force` (cpp:1536) | [`have_dialog_force`] returns [`have_dialog_info`] |
//! | `messages_manager_->force_create_dialog` (cpp:1540) | [`force_create_dialog`] is a no-op |
//! | `messages_manager_->is_dialog_opened` (cpp:1685) | `false`, so [`on_dialog_info_full_invalidated`] is a no-op |
//! | `messages_manager_->get_chat_id_object` (cpp:1690) | returns `dialog_id.get()` (the C++ return value; the dialog-lookup logging is not reachable) |
//! | `user_manager_->reload_user` / `chat_manager_->reload_chat` / `reload_channel` (cpp:1623-1627) | the promise completes with 500 "Network is not implemented yet" |
//! | `chat_manager_->load_chat_full` / `load_channel_full` (cpp:1636-1641) | the promise completes with 500 "Network is not implemented yet" |
//! | `user_manager_->reload_user_full` / `chat_manager_->reload_chat_full` / `reload_channel_full` (cpp:1664-1669) | a logged no-op |
//! | `dialog_manager_->search_public_dialog` / `get_resolved_dialog_by_username` (RecentDialogList.cpp:117/:142) | unresolved: `@username` entries drop on load |
//! | `messages_manager_->load_dialogs` / `get_dialogs_from_list` + `user_manager_->search_contacts` (RecentDialogList.cpp:121-136) | skipped — the load completes from memory alone |
//! | `chat_manager_->get_input_peer_chat` / `get_input_peer_channel` (cpp:1372-1375) | `None` (the C++ `nullptr`) — ChatManager peers arrive with its read surface |
//! | `chat_manager_->have_input_peer_chat` / `have_input_peer_channel` (cpp:1473-1476) | `false` |
//! | `chat_manager_->get_channel_type` (cpp:1498) | `ChannelType::Unknown`, so [`can_send_message_to_dialog`] and the broadcast predicates treat every channel as unloaded |
//! | `chat_manager_->have_chat` / `have_channel` / `have_chat_force` / `have_channel_force` (cpp:1548-1560, :1583-1595) | `false` |
//! | `chat_manager_->is_chat_received_from_server` / `is_channel_received_from_server` (cpp:1587-1589) | `false` |
//! | `chat_manager_->get_basic_group_id_object` / `get_supergroup_id_object` (cpp:1713-1720) | the id value itself (the C++ return value for an unloaded chat; the unknown-chat side effects are M.5.33+) |
//! | `chat_manager_->get_secret_chat_user_id` channel-side helpers — see [`get_chat_type_object`] |
//! | `chat_manager_->is_megagroup_channel` / `is_broadcast_channel` / `is_forum_channel` / `is_forum_tabs_channel` / `is_monoforum_channel` / `is_admined_monoforum_channel` / `get_channel_status` / `on_get_channel_error` (cpp:1942-2028) | the unloaded-channel C++ results — `false` for the type predicates, `Banned(0, "")` for `get_channel_status` (except the status-message heads of `on_get_channel_error`, which are wired) |
//! | `chat_manager_->get_chat_title` / `get_channel_search_text` (cpp:2258-2266) | `""` (the unloaded-chat C++ result) |
//! | `user_manager_->is_user_received_from_server` (cpp:1585) | wired (`is_user_received_from_server_of`) |
//! | `chat_manager_->get_chat_status` / `get_chat_permissions` / `get_channel_status` / `get_channel_permissions` (cpp:1766/:2360/:2300/:2445) | the unloaded C++ `Banned(0, "")` status, so the creator/admin gates answer the unloaded `false` |
//! | `chat_manager_->get_chat_migrated_to_channel_id` / `have_channel` (cpp:1772/:1788) | invalid / `false` |
//! | `messages_manager_->delete_dialog_history` / `chat_manager_->delete_chat` / `delete_channel` / the `SecretChatsManager::cancel_chat` closure (cpp:2041-2048) | the promise completes with 500 "Network is not implemented yet" |
//! | `chat_manager_->get_chat_has_protected_content` / `get_channel_has_protected_content` (cpp:2285/:2288/:2302/:2305) | `false` |
//! | `messages_manager_->report_dialog_from_action_bar` (cpp:2835) | `{is_reported: false, know_action_bar: false}` (the empty option/text/message argument set is the only caller-visible shape until MessagesManager) |
//! | `messages_manager_->can_report_message` (cpp:2849) | `Ok` (no messages exist until MessagesManager) |
//! | the peer-settings/report/blocked/toggle query actors (`UpdatePeerSettingsQuery`, `ReportPeerQuery`, `ReportProfilePhotoQuery`, `ReportEncryptedSpamQuery`, `GetPeerSettingsQuery`, `GetBlockedDialogsQuery`, `GetDialogUnreadMarksQuery`, `ReorderPinnedDialogsQuery`, `ToggleDialogIsBlockedQuery`, `ToggleDialogUnreadMarkQuery`, `ToggleDialogPinQuery`, `ToggleDialogTranslationsQuery`, `ToggleViewForumAsMessagesQuery`, `EditChatDefaultBannedRightsQuery`, `ToggleNoForwardsQuery`, `EditDialogTitleQuery`, `EditDialogPhotoQuery`, `SetChatAvailableReactionsQuery`, `SaveDefaultSendAsQuery`, `EditPeerFoldersQuery`, `SetHistoryTtlQuery`, `SetChatThemeQuery`, `MigrateChatQuery`, `SetChatPhotoQuery` upload tail) | the send is `[blocked: net]` — the promise completes with 500 "Network is not implemented yet"; the `on_result`/`on_error` halves land with the net step |
//! | the erase-log-event promise (`get_erase_log_event_promise`) | not wired — the C++ erases the mirror on settlement, success or error (LogEventHelper.cpp:42-52); the Rust mirrors stay until the net step can complete them (net-step residue: on error paths the Rust re-saves where the C++ erases) |
//! | `get_channel_first_username` (RecentDialogList.cpp:57) | channel dialogs persist by id — the C++ `@username` form arrives with the ChatManager username read |
//! | `file_manager_` file views / `resume_upload` + `UploadDialogPhotoCallback` + `being_uploaded_dialog_photos_` (cpp:2398-2406/:2498-2593) | `[blocked: files]` — set_dialog_photo ports the gates and the empty/previous-photo validation, then reaches the net-blocked query |
//! | `messages_manager_->create_dialog` / `sort_dialogs_by_order` / `can_add_dialog_to_filter` (cpp:3291/:3414/:3385) | local no-op completion / unsorted truncation / `Ok` |
//! | `chat_manager_->reload_channel_full` (cpp:3295) | the promise completes with 500 "Network is not implemented yet" |
//! | `user_manager_->on_update_user_commands` / `chat_manager_->on_update_chat_bot_commands` / `on_update_channel_bot_commands` (cpp:2991/:2995/:2997) | commented no-ops (BotCommands arrives with the M.5/M.8 integration) |
//! | `CheckUsernameQuery` / `CheckChannelUsernameQuery` (cpp:3096-3098) | the send is `[blocked: net]` (see [`check_dialog_username`]) |
//! | `ResolveUsernameQuery` + the `resolve_dialog_username_queries_` queue (cpp:3134-3145) | `[blocked: net]` — the promise completes inline instead of parking in the queue; `resolve_dialog` itself (cpp:3185-3207) lands with the net step |
//! | `SearchPublicDialogsQuery` / `send_search_public_dialogs_query` (cpp:3436-3446) | the send is `[blocked: net]` (see [`search_public_dialogs`] / [`search_dialogs_on_server`]) |
//! | `get_input_dialog_id` (cpp:1340-1348) | lands with the `InputDialogId` type |
//! | `set_dialog_location` (cpp:2782-2799) | lands with the M.5.5+ ChatManager integration (the `DialogLocation` record) |
//! | `hangup` / `tear_down` (cpp:1324-1334) | not ported — the Rust search promises complete inline, so nothing is parked to fail at close, and there is no actor to tear down |

use std::collections::HashMap;

use tdutils::promise::Promise;
use tdutils::status::{Result, Status};
use tdutils::time;

use crate::chat_manager::ChannelType;
use crate::dialog_id::DialogId;
use crate::misc::{clean_username, get_valid_short_usernames};
use crate::td::Td;
use crate::user_manager::{AccessRights, UserEnv};
use tdutils::hints::Hints;

/// `DialogManager::MAX_RECENT_DIALOGS` (DialogManager.h:339).
pub const MAX_RECENT_DIALOGS: usize = 50;

/// `NotificationSettingsScope` (NotificationSettingsScope.h) — moves beside
/// the notification types when their manager lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSettingsScope {
    Private,
    Group,
    Channel,
}

/// `DialogManager::DialogTypeFilter` (DialogManager.h) — the chat-type facet
/// of the search queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogTypeFilter {
    None,
    Bot,
    Broadcast,
}

// M.6.15a — input peers and access checks (DialogManager.cpp:1350-1535).

/// `DialogManager::check_dialog_access` (DialogManager.cpp:1350-1360).
pub fn check_dialog_access(
    td: &mut Td,
    dialog_id: DialogId,
    allow_secret_chats: bool,
    access_rights: AccessRights,
    source: &str,
) -> Result<()> {
    if !have_dialog_force(td, dialog_id, source) {
        if !dialog_id.is_valid() {
            return Result::from_status(Status::error(400, "Invalid chat identifier specified"));
        }
        return Result::from_status(Status::error(400, "Chat not found"));
    }
    check_dialog_access_in_memory(td, dialog_id, allow_secret_chats, access_rights)
}

/// `DialogManager::check_dialog_access_in_memory` (DialogManager.cpp:1362-1375).
pub fn check_dialog_access_in_memory(
    td: &mut Td,
    dialog_id: DialogId,
    allow_secret_chats: bool,
    access_rights: AccessRights,
) -> Result<()> {
    if !have_input_peer(td, dialog_id, allow_secret_chats, access_rights) {
        if dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat && !allow_secret_chats {
            return Result::from_status(Status::error(400, "Not supported in secret chats"));
        }
        if access_rights == AccessRights::Write || access_rights == AccessRights::Edit {
            return Result::from_status(Status::error(400, "Have no write access to the chat"));
        }
        return Result::from_status(Status::error(400, "Can't access the chat"));
    }
    Result::Ok(())
}

/// `DialogManager::get_input_peer` (DialogManager.cpp:1377-1395).
pub fn get_input_peer(
    td: &mut Td,
    dialog_id: DialogId,
    access_rights: AccessRights,
) -> Option<tdmtproto::telegram_api::InputPeer> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let user_id = dialog_id.get_user_id();
            let options = td.option_manager.as_mut()?;
            let env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .get_input_peer_user(&env, user_id, access_rights)
        }
        crate::dialog_id::DialogType::Chat => td
            .chat_manager
            .get_input_peer_chat(dialog_id.get_chat_id(), access_rights),
        crate::dialog_id::DialogType::Channel => {
            let channel_id = dialog_id.get_channel_id();
            let options = td.option_manager.as_mut()?;
            let env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.chat_manager
                .get_input_peer_channel(&env, channel_id, access_rights)
        }
        crate::dialog_id::DialogType::SecretChat => None,
        crate::dialog_id::DialogType::None => {
            Some(tdmtproto::telegram_api::InputPeer::inputPeerEmpty(
                tdmtproto::telegram_api::inputPeerEmpty {},
            ))
        }
    }
}

/// `DialogManager::get_input_peer_force` (DialogManager.cpp:1397-1418).
pub fn get_input_peer_force(dialog_id: DialogId) -> tdmtproto::telegram_api::InputPeer {
    use tdmtproto::telegram_api;
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let user_id = dialog_id.get_user_id();
            telegram_api::InputPeer::inputPeerUser(telegram_api::inputPeerUser {
                user_id: user_id.get(),
                access_hash: 0,
            })
        }
        crate::dialog_id::DialogType::Chat => {
            let chat_id = dialog_id.get_chat_id();
            telegram_api::InputPeer::inputPeerChat(telegram_api::inputPeerChat {
                chat_id: chat_id.get(),
            })
        }
        crate::dialog_id::DialogType::Channel => {
            let channel_id = dialog_id.get_channel_id();
            telegram_api::InputPeer::inputPeerChannel(telegram_api::inputPeerChannel {
                channel_id: channel_id.get(),
                access_hash: 0,
            })
        }
        crate::dialog_id::DialogType::SecretChat | crate::dialog_id::DialogType::None => {
            telegram_api::InputPeer::inputPeerEmpty(telegram_api::inputPeerEmpty {})
        }
    }
}

/// `DialogManager::get_input_peers` (DialogManager.cpp:1420-1433).
pub fn get_input_peers(
    td: &mut Td,
    dialog_ids: &[DialogId],
    access_rights: AccessRights,
) -> Vec<tdmtproto::telegram_api::InputPeer> {
    let mut input_peers = Vec::with_capacity(dialog_ids.len());
    for &dialog_id in dialog_ids {
        if let Some(input_peer) = get_input_peer(td, dialog_id, access_rights) {
            input_peers.push(input_peer);
        } else {
            tdutils::log_error!("Have no access to {:?}", dialog_id);
        }
    }
    input_peers
}

/// `DialogManager::get_input_dialog_peer` (DialogManager.cpp:1435-1448).
pub fn get_input_dialog_peer(
    td: &mut Td,
    dialog_id: DialogId,
    access_rights: AccessRights,
) -> Option<tdmtproto::telegram_api::InputDialogPeer> {
    use tdmtproto::telegram_api;
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::Channel
        | crate::dialog_id::DialogType::None => {
            let peer = get_input_peer(td, dialog_id, access_rights)?;
            Some(telegram_api::InputDialogPeer::inputDialogPeer(
                telegram_api::inputDialogPeer {
                    peer: Some(Box::new(peer)),
                },
            ))
        }
        crate::dialog_id::DialogType::SecretChat => None,
    }
}

/// `DialogManager::get_input_dialog_peers` (DialogManager.cpp:1450-1466).
pub fn get_input_dialog_peers(
    td: &mut Td,
    dialog_ids: &[DialogId],
    access_rights: AccessRights,
) -> Vec<tdmtproto::telegram_api::InputDialogPeer> {
    let mut input_dialog_peers = Vec::with_capacity(dialog_ids.len());
    for &dialog_id in dialog_ids {
        if let Some(input_dialog_peer) = get_input_dialog_peer(td, dialog_id, access_rights) {
            input_dialog_peers.push(input_dialog_peer);
        } else {
            tdutils::log_error!("Have no access to {:?}", dialog_id);
        }
    }
    input_dialog_peers
}

/// `DialogManager::get_input_encrypted_chat` (DialogManager.cpp:1468-1478).
pub fn get_input_encrypted_chat(
    td: &Td,
    dialog_id: DialogId,
    access_rights: AccessRights,
) -> Option<tdmtproto::telegram_api::InputEncryptedChat> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_input_encrypted_chat(dialog_id.get_secret_chat_id(), access_rights),
        // UNREACHABLE in the C++
        _ => None,
    }
}

/// `DialogManager::have_input_peer` (DialogManager.cpp:1480-1505).
pub fn have_input_peer(
    td: &mut Td,
    dialog_id: DialogId,
    allow_secret_chats: bool,
    access_rights: AccessRights,
) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let user_id = dialog_id.get_user_id();
            let Some(options) = td.option_manager.as_mut() else {
                return false;
            };
            let env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .have_input_peer_user(&env, user_id, access_rights)
        }
        // ChatManager peers arrive with its read surface (stub inventory).
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
        crate::dialog_id::DialogType::SecretChat => {
            if !allow_secret_chats {
                return false;
            }
            td.user_manager
                .have_input_encrypted_peer(dialog_id.get_secret_chat_id(), access_rights)
        }
        crate::dialog_id::DialogType::None => false,
    }
}

/// `DialogManager::can_send_message_to_dialog` (DialogManager.cpp:1507-1531).
/// The channel branch reads `get_channel_type`, which is the unloaded
/// `Unknown` until the ChatManager read surface lands (stub inventory), so
/// the broadcast check can never fire yet.
pub fn can_send_message_to_dialog(td: &mut Td, dialog_id: DialogId) -> Result<()> {
    if !have_input_peer(td, dialog_id, true, AccessRights::Write) {
        return Result::from_status(Status::error(400, "Have no write access to the chat"));
    }

    if dialog_id.get_type() == crate::dialog_id::DialogType::Channel {
        let channel_id = dialog_id.get_channel_id();
        let channel_type = get_channel_type(channel_id);
        let channel_permissions = get_channel_permissions(channel_id);

        match channel_type {
            ChannelType::Unknown | ChannelType::Megagroup => {}
            ChannelType::Broadcast => {
                if !channel_permissions.can_post_messages() {
                    return Result::from_status(Status::error(
                        400,
                        "Need administrator rights in the channel chat",
                    ));
                }
            }
        }
    }
    Result::Ok(())
}

/// `ChatManager::get_channel_type` — unloaded until the channel registry
/// lands (stub inventory).
fn get_channel_type(_channel_id: crate::chat_manager::ChannelId) -> ChannelType {
    ChannelType::Unknown
}

/// `ChatManager::get_channel_permissions` (ChatManager.cpp:8693-8699) — the
/// unloaded-channel `Banned(0, "")` (stub inventory).
fn get_channel_permissions(
    _channel_id: crate::chat_manager::ChannelId,
) -> crate::chat_manager::DialogParticipantStatus {
    crate::chat_manager::DialogParticipantStatus::default()
}

// M.6.15b — dialog info reload + force create (DialogManager.cpp:1536-1752).

/// `DialogManager::have_dialog_force` (DialogManager.cpp:1536-1538) — the
/// MessagesManager call is stubbed to the in-memory [`have_dialog_info`]
/// (stub inventory).
pub fn have_dialog_force(td: &mut Td, dialog_id: DialogId, _source: &str) -> bool {
    have_dialog_info(td, dialog_id)
}

/// `DialogManager::force_create_dialog` (DialogManager.cpp:1540-1543) — the
/// MessagesManager call is stubbed to a no-op (stub inventory); the
/// `expect_no_access` / `force_update_dialog_pos` args ride the stub.
pub fn force_create_dialog(_td: &mut Td, _dialog_id: DialogId, _source: &str) {
    // M.6 stub: messages_manager_->force_create_dialog
}

/// `DialogManager::get_peers_dialog_ids` (DialogManager.cpp:1545-1557) —
/// [`force_create_dialog`] is the documented no-op (stub inventory).
pub fn get_peers_dialog_ids(
    td: &mut Td,
    peers: &[tdmtproto::telegram_api::Peer],
    _expect_no_access: bool,
) -> Vec<DialogId> {
    let mut result = Vec::with_capacity(peers.len());
    for peer in peers {
        let dialog_id = DialogId::from_peer(peer);
        if dialog_id.is_valid() {
            force_create_dialog(td, dialog_id, "get_peers_dialog_ids");
            result.push(dialog_id);
        }
    }
    result
}

/// `DialogManager::have_dialog_info` (DialogManager.cpp:1559-1581).
pub fn have_dialog_info(td: &Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .have_accessible_user(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat => td.chat_manager.have_chat(dialog_id.get_chat_id()),
        crate::dialog_id::DialogType::Channel => {
            td.chat_manager.have_channel(dialog_id.get_channel_id())
        }
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .have_secret_chat(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => false,
    }
}

/// `DialogManager::is_dialog_info_received_from_server`
/// (DialogManager.cpp:1583-1594); the chat/channel halves are stubbed to
/// `false` (stub inventory).
pub fn is_dialog_info_received_from_server(td: &Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .is_user_received_from_server_of(dialog_id.get_user_id()),
        _ => false,
    }
}

/// `DialogManager::have_dialog_info_force` (DialogManager.cpp:1596-1618);
/// the chat/channel halves are stubbed to `false` (stub inventory). The C++
/// forwards the caller's `source` into the `*_force` helpers.
pub fn have_dialog_info_force(td: &mut Td, dialog_id: DialogId, source: &str) -> bool {
    let Some(options) = td.option_manager.as_mut() else {
        return false;
    };
    let mut env = UserEnv {
        global: &mut td.global,
        options,
        auth_is_bot: td.auth_manager.is_bot(),
    };
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            td.user_manager
                .have_user_force(&mut env, dialog_id.get_user_id(), source)
        }
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
        crate::dialog_id::DialogType::SecretChat => {
            td.user_manager
                .have_secret_chat_force(&mut env, dialog_id.get_secret_chat_id(), source)
        }
        crate::dialog_id::DialogType::None => false,
    }
}

/// `DialogManager::reload_dialog_info` (DialogManager.cpp:1620-1630); every
/// reachable branch needs a server query (stub inventory). The default arm
/// is the C++ `promise.set_error("Invalid chat identifier to reload")` —
/// the Slice overload, `Status::Error(0, message)` (Status.h:181-183).
pub fn reload_dialog_info(_td: &mut Td, dialog_id: DialogId, mut promise: Promise<()>) {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::Channel => {
            promise.set_error(Status::error(500, "Network is not implemented yet"));
        }
        _ => promise.set_error(Status::error_message("Invalid chat identifier to reload")),
    }
}

/// `DialogManager::get_dialog_info_full` (DialogManager.cpp:1633-1654). The
/// C++ resolves the promise from the async load; here the user-full load
/// runs inline (the M.4.16 park contract applies inside). The C++ forwards
/// the caller's `source` into the load.
pub fn get_dialog_info_full(
    td: &mut Td,
    dialog_id: DialogId,
    mut promise: Promise<()>,
    source: &str,
) {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let user_id = dialog_id.get_user_id();
            let Some(options) = td.option_manager.as_mut() else {
                promise.set_value(());
                return;
            };
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            match td
                .user_manager
                .load_user_full(&mut env, user_id, false, source)
            {
                Ok(_) => promise.set_value(()),
                Err(message) => promise.set_error(Status::error(400, message)),
            }
        }
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            promise.set_error(Status::error(500, "Network is not implemented yet"));
        }
        crate::dialog_id::DialogType::SecretChat => promise.set_value(()),
        crate::dialog_id::DialogType::None => {
            promise.set_error(Status::error(500, "Wrong chat type"));
        }
    }
}

/// `DialogManager::on_dialog_info_full_invalidated` (DialogManager.cpp:1684-1686)
/// — the `is_dialog_opened` MessagesManager call is stubbed to `false`
/// (stub inventory).
pub fn on_dialog_info_full_invalidated(_td: &mut Td, _dialog_id: DialogId) {
    // M.6 stub: messages_manager_->is_dialog_opened gates the full-info
    // reload, which lands with the user/chat/channel full-reload
    // families (§3.3/§3.4).
}

// M.6.16 — builders, predicates and error mapping.

/// `DialogManager::get_chat_id_object` (DialogManager.cpp:1690-1692) — the
/// MessagesManager lookup is stubbed to the C++ return value (stub
/// inventory).
pub fn get_chat_id_object(dialog_id: DialogId, _source: &str) -> i64 {
    dialog_id.get()
}

/// `DialogManager::get_chat_ids_object` (DialogManager.cpp:1694-1696).
pub fn get_chat_ids_object(dialog_ids: &[DialogId], source: &str) -> Vec<i64> {
    dialog_ids
        .iter()
        .map(|&dialog_id| get_chat_id_object(dialog_id, source))
        .collect()
}

/// `DialogManager::get_chats_object` (DialogManager.cpp:1698-1703).
pub fn get_chats_object(
    total_count: i32,
    dialog_ids: &[DialogId],
    source: &str,
) -> crate::td_api::Chats {
    let total_count = if total_count == -1 {
        dialog_ids.len() as i32
    } else {
        total_count
    };
    crate::td_api::Chats {
        total_count,
        chat_ids: get_chat_ids_object(dialog_ids, source),
    }
}

/// The pair overload `DialogManager::get_chats_object(const
/// std::pair<int32, vector<DialogId>> &, const char *)`
/// (DialogManager.cpp:1706-1709) — a one-line delegation in the C++; the
/// Rust callers destructure, so this is API-surface completeness.
#[allow(dead_code)]
pub fn get_chats_object_from_pair(
    (total_count, dialog_ids): (i32, Vec<DialogId>),
    source: &str,
) -> crate::td_api::Chats {
    get_chats_object(total_count, &dialog_ids, source)
}

/// `DialogManager::get_chat_type_object` (DialogManager.cpp:1711-1737); the
/// basic-group/supergroup id getters are stubbed to the id value — the C++
/// return value for an unloaded chat (stub inventory).
pub fn get_chat_type_object(
    td: &mut Td,
    dialog_id: DialogId,
    source: &str,
) -> crate::td_api::ChatType {
    use crate::td_api;
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let user_id = dialog_id.get_user_id();
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td_api::ChatType::chatTypePrivate(td_api::chatTypePrivate {
                user_id: td
                    .user_manager
                    .get_user_id_object(&mut env, user_id, source),
            })
        }
        crate::dialog_id::DialogType::Chat => {
            let chat_id = dialog_id.get_chat_id();
            td_api::ChatType::chatTypeBasicGroup(td_api::chatTypeBasicGroup {
                basic_group_id: chat_id.get(),
            })
        }
        crate::dialog_id::DialogType::Channel => {
            let channel_id = dialog_id.get_channel_id();
            td_api::ChatType::chatTypeSupergroup(td_api::chatTypeSupergroup {
                supergroup_id: channel_id.get(),
                is_channel: !is_megagroup_channel(channel_id),
            })
        }
        crate::dialog_id::DialogType::SecretChat => {
            let secret_chat_id = dialog_id.get_secret_chat_id();
            let user_id = td.user_manager.get_secret_chat_user_id(secret_chat_id);
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td_api::ChatType::chatTypeSecret(td_api::chatTypeSecret {
                secret_chat_id: td
                    .user_manager
                    .get_secret_chat_id_object(secret_chat_id, source),
                user_id: td
                    .user_manager
                    .get_user_id_object(&mut env, user_id, source),
            })
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_chat_type_object on an invalid dialog")
        }
    }
}

/// `DialogManager::get_dialog_notification_setting_scope`
/// (DialogManager.cpp:1739-1752); the channel branch reads the broadcast
/// predicate, which is `false` for unloaded channels (stub inventory). The
/// `None` arm is the C++ `UNREACHABLE()`.
pub fn get_dialog_notification_setting_scope(
    td: &mut Td,
    dialog_id: DialogId,
) -> NotificationSettingsScope {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User | crate::dialog_id::DialogType::SecretChat => {
            NotificationSettingsScope::Private
        }
        crate::dialog_id::DialogType::Chat => NotificationSettingsScope::Group,
        crate::dialog_id::DialogType::Channel => {
            if is_broadcast_channel(td, dialog_id) {
                NotificationSettingsScope::Channel
            } else {
                NotificationSettingsScope::Group
            }
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_notification_setting_scope on DialogType::None")
        }
    }
}

/// `DialogManager::is_anonymous_administrator` (DialogManager.cpp:1917-1940);
/// the channel-state reads are stubbed to the unloaded results (stub
/// inventory), so only the broadcast/admined-monoforum and bot branches can
/// answer. The `author_signature` out-param (`status.get_rank()`,
/// cpp:1935-1937) is dropped: the stub never reaches the anonymous arm and
/// the in-region C++ caller passes nullptr; it returns with the
/// MessagesManager signed-message callers.
pub fn is_anonymous_administrator(td: &mut Td, dialog_id: DialogId) -> bool {
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        // C++ `CHECK(dialog_id.is_valid())`
        if !dialog_id.is_valid() {
            panic!("CHECK failed: dialog_id.is_valid()");
        }
        return false;
    }

    // get_channel_type(channel_id) == Broadcast ||
    // get_channel_status(channel_id).is_admined_monoforum — both are the
    // unloaded `false` until the channel registry lands (stub inventory)
    if td.auth_manager.is_bot() {
        return false;
    }

    // get_channel_status(channel_id).is_anonymous() — unloaded → false
    false
}

/// `DialogManager::is_group_dialog` (DialogManager.cpp:1942-1951).
pub fn is_group_dialog(_td: &mut Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::Chat => true,
        crate::dialog_id::DialogType::Channel => is_megagroup_channel(dialog_id.get_channel_id()),
        _ => false,
    }
}

/// `ChatManager::is_megagroup_channel` — unloaded → `Unknown` → `false`
/// (stub inventory).
pub fn is_megagroup_channel(_channel_id: crate::chat_manager::ChannelId) -> bool {
    false
}

/// `DialogManager::is_forum_channel` (DialogManager.cpp:1953-1956) — the
/// channel read is stubbed to `false` (stub inventory).
pub fn is_forum_channel(td: &mut Td, dialog_id: DialogId) -> bool {
    let _ = td;
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return false;
    }
    false
}

/// `DialogManager::is_forum_tabs_channel` (DialogManager.cpp:1958-1961) —
/// stubbed to `false` (stub inventory).
pub fn is_forum_tabs_channel(td: &mut Td, dialog_id: DialogId) -> bool {
    let _ = td;
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return false;
    }
    false
}

/// `DialogManager::is_monoforum_channel` (DialogManager.cpp:1963-1966) —
/// stubbed to `false` (stub inventory).
pub fn is_monoforum_channel(td: &mut Td, dialog_id: DialogId) -> bool {
    let _ = td;
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return false;
    }
    false
}

/// `DialogManager::is_admined_monoforum_channel` (DialogManager.cpp:1968-1971)
/// — stubbed to `false` (stub inventory).
pub fn is_admined_monoforum_channel(td: &mut Td, dialog_id: DialogId) -> bool {
    let _ = td;
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return false;
    }
    false
}

/// `DialogManager::is_broadcast_channel` (DialogManager.cpp:1973-1982) —
/// the channel read is stubbed to `false` (stub inventory).
pub fn is_broadcast_channel(td: &mut Td, dialog_id: DialogId) -> bool {
    let _ = td;
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return false;
    }
    false
}

/// `DialogManager::can_dialog_have_threads` (DialogManager.cpp:1981-1994).
pub fn can_dialog_have_threads(td: &mut Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            td.auth_manager.is_bot() || td.user_manager.is_user_bot_of(dialog_id.get_user_id())
        }
        crate::dialog_id::DialogType::Channel => !is_broadcast_channel(td, dialog_id),
        _ => false,
    }
}

/// `DialogManager::on_get_dialog_error` (DialogManager.cpp:1996-2028); the
/// channel arm ports the status-message heads of
/// `chat_manager_->on_get_channel_error` (ChatManager.cpp:6395-6400) — the
/// channel-registry halves are the unloaded result (stub inventory).
/// `BOT_METHOD_INVALID` is intercepted at the dialog level first, exactly
/// like the C++.
pub fn on_get_dialog_error(
    td: &mut Td,
    dialog_id: DialogId,
    status: &Status,
    _source: &str,
) -> bool {
    let message = status.message();
    if message == "BOT_METHOD_INVALID" {
        return true;
    }
    if td.global.is_expected_error(status) {
        return true;
    }
    if message == "SEND_AS_PEER_INVALID" {
        // C++: reload_dialog_info_full (DialogManager.cpp:1656-1682)
        // here — the full-info reload lands with the user/chat/channel
        // full-reload families (§3.3/§3.4).
        return true;
    }
    if message == "QUOTE_TEXT_INVALID"
        || message == "REPLY_MESSAGE_ID_INVALID"
        || message.starts_with("ALLOW_PAYMENT_REQUIRED_")
    {
        return true;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::SecretChat
        | crate::dialog_id::DialogType::None => false,
        // ChatManager::on_get_channel_error — `CHANNEL_MONOFORUM_UNSUPPORTED`
        // answers `true` before any channel lookup; the channel-registry
        // remainder is the unloaded `false` (stub inventory). Only regular
        // channels process errors (e962a0799) — a community (the
        // monoforum id band) breaks out to `false`.
        crate::dialog_id::DialogType::Channel => {
            dialog_id.get_channel_id().is_regular_channel()
                && message == "CHANNEL_MONOFORUM_UNSUPPORTED"
        }
    }
}

/// `DialogManager::get_dialog_search_text` (DialogManager.cpp:2256-2274);
/// the chat/channel halves return the unloaded-title `""` (stub inventory).
pub fn get_dialog_search_text(td: &mut Td, dialog_id: DialogId) -> String {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_search_text_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => String::new(),
        crate::dialog_id::DialogType::SecretChat => {
            let user_id = td
                .user_manager
                .get_secret_chat_user_id(dialog_id.get_secret_chat_id());
            td.user_manager.get_user_search_text_of(user_id)
        }
        crate::dialog_id::DialogType::None => String::new(),
    }
}

// M.6.17 — recently found/opened + hints (DialogManager.cpp:1811-1941,
// RecentDialogList.cpp).

/// `RecentDialogList` (RecentDialogList.h) — the load is synchronous here
/// (the pmc read needs no actor); the server-side resolution stand-ins are
/// inventoried at the module head.
pub struct RecentDialogList {
    name: &'static str,
    max_size: usize,
    dialog_ids: Vec<DialogId>,
    removed_dialog_ids: Vec<DialogId>,
    is_loaded: bool,
}

impl RecentDialogList {
    /// `RecentDialogList(Td *, name, max_size)` (RecentDialogList.cpp:34-37);
    /// the actor registration collapses with the synchronous load.
    pub fn new(name: &'static str, max_size: usize) -> RecentDialogList {
        RecentDialogList {
            name,
            max_size,
            dialog_ids: Vec::new(),
            removed_dialog_ids: Vec::new(),
            is_loaded: false,
        }
    }

    /// `get_binlog_key` (RecentDialogList.cpp:39-41).
    fn get_binlog_key(&self) -> String {
        format!("{}_dialog_usernames_and_ids", self.name)
    }

    /// `save_dialogs` (RecentDialogList.cpp:43-84). The
    /// `!use_chat_info_database` branch prefers usernames: the user half is
    /// wired, the channel half stays id-only until the ChatManager username
    /// read (`get_channel_first_username`, RecentDialogList.cpp:57) lands
    /// (stub inventory).
    fn save_dialogs(&self, td: &mut Td) {
        if !self.is_loaded {
            return;
        }
        // C++ `CHECK(removed_dialog_ids_.empty())`
        if !self.removed_dialog_ids.is_empty() {
            panic!("CHECK failed: removed_dialog_ids_.empty()");
        }

        let use_chat_info_database = td.global.td_db().parameters().use_chat_info_database;
        let mut result = String::new();
        for &dialog_id in &self.dialog_ids {
            result.push(',');
            if !use_chat_info_database {
                let username = match dialog_id.get_type() {
                    crate::dialog_id::DialogType::User => {
                        // the C++ single-arg is_user_contact is is_contact
                        // only (UserManager.h:249 — the is_mutual default
                        // is false)
                        if !td
                            .user_manager
                            .is_user_contact_of(dialog_id.get_user_id(), false)
                        {
                            td.user_manager
                                .get_user_first_username_of(dialog_id.get_user_id())
                                .to_string()
                        } else {
                            String::new()
                        }
                    }
                    _ => String::new(),
                };
                if !username.is_empty() && !username.contains(',') {
                    result.push('@');
                    result.push_str(&username);
                    continue;
                }
            }
            result.push_str(&dialog_id.get().to_string());
        }
        if !result.is_empty() {
            result.remove(0);
        }
        td.global.binlog_pmc().set(&self.get_binlog_key(), &result);
    }

    /// `load_dialogs` + `on_load_dialogs` (RecentDialogList.cpp:86-162) —
    /// one synchronous pass; the server-resolution branches drop their
    /// entries (stub inventory).
    fn load_dialogs(&mut self, td: &mut Td) {
        if self.is_loaded {
            return;
        }

        let saved = td.global.binlog_pmc().get(&self.get_binlog_key());
        let found_dialogs = full_split(&saved, ',');

        if td.global.close_flag() {
            return;
        }

        let newly_found_dialogs = std::mem::take(&mut self.dialog_ids);
        self.dialog_ids = Vec::new();

        for found_dialog in found_dialogs.iter().rev() {
            let dialog_id = if let Some(username) = found_dialog.strip_prefix('@') {
                // M.6 stub: get_resolved_dialog_by_username ([blocked: net])
                let _ = username;
                DialogId::new(0)
            } else {
                DialogId::new(found_dialog.parse::<i64>().unwrap_or(0))
            };
            if dialog_id.is_valid()
                && !self.removed_dialog_ids.contains(&dialog_id)
                && have_dialog_info(td, dialog_id)
                && have_input_peer(td, dialog_id, true, AccessRights::Read)
            {
                force_create_dialog(td, dialog_id, "recent dialog");
                self.do_add_dialog(dialog_id);
            }
        }
        for &dialog_id in newly_found_dialogs.iter().rev() {
            self.do_add_dialog(dialog_id);
        }
        self.is_loaded = true;
        self.removed_dialog_ids.clear();
        if !newly_found_dialogs.is_empty() {
            self.save_dialogs(td);
        }
    }

    /// `do_add_dialog` (RecentDialogList.cpp:180-190).
    fn do_add_dialog(&mut self, dialog_id: DialogId) -> bool {
        if !self.dialog_ids.is_empty() && self.dialog_ids[0] == dialog_id {
            return false;
        }

        add_to_top(&mut self.dialog_ids, self.max_size, dialog_id);

        self.removed_dialog_ids.retain(|&id| id != dialog_id);
        true
    }

    /// `add_dialog` (RecentDialogList.cpp:164-170).
    pub fn add_dialog(&mut self, td: &mut Td, dialog_id: DialogId) {
        self.load_dialogs(td);
        if self.do_add_dialog(dialog_id) {
            self.save_dialogs(td);
        }
    }

    /// `remove_dialog` (RecentDialogList.cpp:192-204).
    pub fn remove_dialog(&mut self, td: &mut Td, dialog_id: DialogId) {
        if !dialog_id.is_valid() {
            return;
        }
        self.load_dialogs(td);
        if let Some(position) = self.dialog_ids.iter().position(|&id| id == dialog_id) {
            self.dialog_ids.remove(position);
            self.save_dialogs(td);
        } else if !self.is_loaded && !self.removed_dialog_ids.contains(&dialog_id) {
            self.removed_dialog_ids.push(dialog_id);
        }
    }

    /// `update_dialogs` (RecentDialogList.cpp:206-244) — the
    /// MessagesManager liveness check is stubbed to [`have_dialog_info`];
    /// the migration and deleted-secret-chat checks collapse (stub
    /// inventory).
    fn update_dialogs(&mut self, td: &mut Td) {
        // C++ `CHECK(is_loaded_)`
        if !self.is_loaded {
            panic!("CHECK failed: is_loaded_");
        }
        let mut dialog_ids = Vec::new();
        for &dialog_id in &self.dialog_ids {
            if have_dialog_info(td, dialog_id) {
                dialog_ids.push(dialog_id);
            }
        }

        if dialog_ids != self.dialog_ids {
            self.dialog_ids = dialog_ids;
            self.save_dialogs(td);
        }
    }

    /// `get_dialogs` (RecentDialogList.cpp:240-255) — the load promise
    /// collapses into the synchronous return: the caller resolves its own
    /// promise once this returns (the load is always finished). The
    /// close-flagged load aborts like the C++ `on_load_dialogs`
    /// `fail_promises(promises, Global::request_aborted_error())`
    /// (RecentDialogList.cpp:131-132): the callers complete the promise
    /// with 500 "Request aborted" when `is_loaded` is still false.
    fn get_dialogs(&mut self, td: &mut Td, limit: i32) -> (i32, Vec<DialogId>) {
        self.load_dialogs(td);
        if !self.is_loaded {
            return (0, Vec::new());
        }

        self.update_dialogs(td);

        // C++ `CHECK(limit >= 0)`
        if limit < 0 {
            panic!("CHECK failed: limit >= 0");
        }
        let total_count = self.dialog_ids.len() as i32;
        (
            total_count,
            self.dialog_ids[..(limit as usize).min(self.dialog_ids.len())].to_vec(),
        )
    }

    /// `clear_dialogs` (RecentDialogList.cpp:257-264).
    fn clear_dialogs(&mut self, td: &mut Td) {
        if self.dialog_ids.is_empty() && self.is_loaded {
            return;
        }

        self.is_loaded = true;
        self.dialog_ids.clear();
        self.removed_dialog_ids.clear();
        self.save_dialogs(td);
    }
}

/// `td::add_to_top` (td/utils/algorithm.h) — move the element to the front,
/// truncating the list at `max_size`.
fn add_to_top(list: &mut Vec<DialogId>, max_size: usize, dialog_id: DialogId) {
    if let Some(position) = list.iter().position(|&id| id == dialog_id) {
        list.remove(position);
    }
    list.insert(0, dialog_id);
    list.truncate(max_size);
}

/// `td::full_split` (tdutils/td/utils/misc.h:35-50). The C++ returns an
/// empty vector for an empty string (`if (s.empty()) { return result; }`);
/// Rust's `split` yields one empty part, which every consumer here skips
/// (the load-time `is_valid` filter), so the observable behavior matches.
fn full_split(s: &str, sep: char) -> Vec<String> {
    s.split(sep).map(str::to_string).collect()
}

/// `DialogManager::ResolvedUsername` (DialogManager.h) — a username resolve
/// cache entry; `expires_at == 0` means loaded from the database (never
/// refreshed from memory).
pub struct ResolvedUsername {
    pub(crate) dialog_id: DialogId,
    pub(crate) expires_at: f64,
}

/// `DialogManager::USERNAME_CACHE_EXPIRE_TIME` (DialogManager.h:341).
/// Consumed by the username intakes below; their callers arrive with the
/// UserManager/ChatManager username fan-ins.
#[allow(dead_code)]
const USERNAME_CACHE_EXPIRE_TIME: i64 = 86400;

/// `DialogManager::MIN_SEARCH_PUBLIC_DIALOG_PREFIX_LEN` (DialogManager.h:337)
/// — the server-side prefix limit.
const MIN_SEARCH_PUBLIC_DIALOG_PREFIX_LEN: usize = 4;

/// `DialogManager::MAX_GET_DIALOGS` (DialogManager.h:338) — the server-side
/// limit.
const MAX_GET_DIALOGS: i32 = 100;

/// `ChatManager::MAX_TITLE_LENGTH` (ChatManager.h:678) — the server-side
/// limit for a chat title.
const MAX_TITLE_LENGTH: usize = 128;

/// The dialog-plane in-flight query kinds — the C++ ResultHandler
/// classes of the §2 flip families (DialogManager.cpp). The kinds ride
/// the `DialogManager::net` handle; the answers drain through
/// [`Td::dispatch_completed_dialog_net`](crate::td::Td::dispatch_completed_dialog_net).
#[derive(Clone, Copy, Debug)]
pub(crate) enum DialogNetKind {}

/// `DialogManager` — the state of the C++ DialogManager actor that exists in
/// this slice: the two recent-dialog lists (the ctor, DialogManager.cpp:1311-1312)
/// and the search hints.
pub struct DialogManager {
    pub(crate) recently_found_dialogs: RecentDialogList,
    pub(crate) recently_opened_dialogs: RecentDialogList,
    pub(crate) dialog_hints: Hints,
    /// `resolved_usernames_` (DialogManager.h) — cleaned username → dialog,
    /// with an expiry; fills from server resolves and username updates.
    pub(crate) resolved_usernames: HashMap<String, ResolvedUsername>,
    /// `inaccessible_resolved_usernames_` (DialogManager.h) — resolved to an
    /// existing but unloaded dialog (no expiry).
    pub(crate) inaccessible_resolved_usernames: HashMap<String, DialogId>,
    /// `found_public_dialogs_` / `found_on_server_dialogs_` (DialogManager.h)
    /// — the searchPublicChatsByUsername caches, indexed by the
    /// `DialogTypeFilter` number (the C++ `static_cast<int32>`).
    pub(crate) found_public_dialogs: [HashMap<String, Vec<DialogId>>; 3],
    pub(crate) found_on_server_dialogs: [HashMap<String, Vec<DialogId>>; 3],
    /// `reload_video_chat_on_search_usernames_` (DialogManager.h).
    pub(crate) reload_video_chat_on_search_usernames: std::collections::BTreeSet<String>,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the dispatcher install).
    pub(crate) net: Option<crate::td::DialogNetHandle>,
}

impl Default for DialogManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DialogManager {
    /// `DialogManager::DialogManager` (DialogManager.cpp:1310-1314).
    pub fn new() -> DialogManager {
        DialogManager {
            recently_found_dialogs: RecentDialogList::new("recently_found", MAX_RECENT_DIALOGS),
            recently_opened_dialogs: RecentDialogList::new("recently_opened", MAX_RECENT_DIALOGS),
            dialog_hints: Hints::default(),
            resolved_usernames: HashMap::new(),
            inaccessible_resolved_usernames: HashMap::new(),
            found_public_dialogs: Default::default(),
            found_on_server_dialogs: Default::default(),
            reload_video_chat_on_search_usernames: Default::default(),
            net: None,
        }
    }

    /// `DialogManager::add_dialog_to_hints` (DialogManager.cpp:1811-1813).
    pub(crate) fn add_dialog_to_hints(&mut self, td: &mut Td, dialog_id: DialogId) {
        let text = get_dialog_search_text(td, dialog_id);
        self.dialog_hints.add(-dialog_id.get(), &text);
    }

    /// `DialogManager::update_dialog_hints_rating` (DialogManager.cpp:1815-1826).
    pub(crate) fn update_dialog_hints_rating(&mut self, td: &Td, dialog_id: DialogId, rating: i64) {
        if td.auth_manager.is_bot() {
            return;
        }
        if rating == 0 {
            tracing::info!("Remove {:?} from chat search", dialog_id.get());
            self.dialog_hints.remove(-dialog_id.get());
        } else {
            tracing::info!("Change position of {:?} in chat search", dialog_id.get());
            self.dialog_hints.set_rating(-dialog_id.get(), -rating);
        }
    }

    /// `DialogManager::search_dialogs` (DialogManager.cpp:1828-1860) — the
    /// promise resolves before returning (the load is synchronous).
    pub(crate) fn search_dialogs(
        &mut self,
        td: &mut Td,
        query: &str,
        chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
        limit: i32,
        mut promise: Promise<()>,
    ) -> (i32, Vec<DialogId>) {
        tracing::info!("Search chats with query \"{}\" and limit {}", query, limit);
        // C++ `CHECK(!td_->auth_manager_->is_bot())`
        if td.auth_manager.is_bot() {
            panic!("CHECK failed: search_dialogs is not for bots");
        }

        if limit < 0 {
            promise.set_error(Status::error(400, "Limit must be non-negative"));
            return (0, Vec::new());
        }
        if query.is_empty() {
            return self.search_recently_found_dialogs(td, "", chat_type_filter, limit, promise);
        }
        let type_filter = get_dialog_type_filter(chat_type_filter);

        let hint_limit = if type_filter == DialogTypeFilter::None {
            limit
        } else {
            10000
        };
        let (total_count, keys) = self.dialog_hints.search(query, hint_limit, false);
        let mut keys = keys;
        if type_filter != DialogTypeFilter::None {
            keys.retain(|&key| {
                is_dialog_suitable_for_type_filter(td, DialogId::new(-key), type_filter)
            });
            let total = keys.len() as i32;
            keys.truncate(limit.max(0) as usize);
            let result = (total, keys);
            let dialog_ids: Vec<DialogId> =
                result.1.iter().map(|&key| DialogId::new(-key)).collect();
            promise.set_value(());
            return (result.0, dialog_ids);
        }
        let dialog_ids: Vec<DialogId> = keys.iter().map(|&key| DialogId::new(-key)).collect();

        promise.set_value(());
        (total_count as i32, dialog_ids)
    }

    /// `DialogManager::search_recently_found_dialogs`
    /// (DialogManager.cpp:1862-1890).
    pub(crate) fn search_recently_found_dialogs(
        &mut self,
        td: &mut Td,
        query: &str,
        chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
        limit: i32,
        mut promise: Promise<()>,
    ) -> (i32, Vec<DialogId>) {
        let type_filter = get_dialog_type_filter(chat_type_filter);
        let fetch_limit = if query.is_empty() && type_filter == DialogTypeFilter::None {
            limit
        } else {
            50
        };
        let (mut total_count, mut dialog_ids) =
            self.recently_found_dialogs.get_dialogs(td, fetch_limit);
        if !self.recently_found_dialogs.is_loaded && td.global.close_flag() {
            // Global::request_aborted_error() — the C++ fails the parked
            // load promises with 500 "Request aborted" on close
            promise.set_error(Status::error(500, "Request aborted"));
            return (0, Vec::new());
        }
        if type_filter != DialogTypeFilter::None {
            dialog_ids.retain(|&dialog_id| {
                is_dialog_suitable_for_type_filter(td, dialog_id, type_filter)
            });
            total_count = dialog_ids.len() as i32;
            if query.is_empty() && dialog_ids.len() > limit.max(0) as usize {
                dialog_ids.truncate(limit.max(0) as usize);
            }
        }

        if total_count == 0 || query.is_empty() {
            promise.set_value(());
            return (total_count, dialog_ids);
        }

        let mut hints = Hints::default();
        let mut rating: i64 = 1;
        for &dialog_id in &dialog_ids {
            let text = get_dialog_search_text(td, dialog_id);
            hints.add(dialog_id.get(), &text);
            rating += 1;
            hints.set_rating(dialog_id.get(), rating);
        }

        let (total, keys) = hints.search(query, limit, false);
        promise.set_value(());
        (total as i32, keys.into_iter().map(DialogId::new).collect())
    }

    /// `DialogManager::add_recently_found_dialog` (DialogManager.cpp:1892-1898).
    pub(crate) fn add_recently_found_dialog(
        &mut self,
        td: &mut Td,
        dialog_id: DialogId,
    ) -> Result<()> {
        if !have_dialog_force(td, dialog_id, "add_recently_found_dialog") {
            return Result::from_status(Status::error(400, "Chat not found"));
        }
        self.recently_found_dialogs.add_dialog(td, dialog_id);
        Result::Ok(())
    }

    /// `DialogManager::remove_recently_found_dialog` (DialogManager.cpp:1900-1906).
    pub(crate) fn remove_recently_found_dialog(
        &mut self,
        td: &mut Td,
        dialog_id: DialogId,
    ) -> Result<()> {
        if !have_dialog_force(td, dialog_id, "remove_recently_found_dialog") {
            return Result::from_status(Status::error(400, "Chat not found"));
        }
        self.recently_found_dialogs.remove_dialog(td, dialog_id);
        Result::Ok(())
    }

    /// `DialogManager::clear_recently_found_dialogs` (DialogManager.cpp:1908-1910).
    pub(crate) fn clear_recently_found_dialogs(&mut self, td: &mut Td) {
        self.recently_found_dialogs.clear_dialogs(td);
    }

    /// `DialogManager::get_recently_opened_dialogs` (DialogManager.cpp:1912-1915).
    pub(crate) fn get_recently_opened_dialogs(
        &mut self,
        td: &mut Td,
        limit: i32,
        mut promise: Promise<()>,
    ) -> (i32, Vec<DialogId>) {
        // C++ `CHECK(!td_->auth_manager_->is_bot())`
        if td.auth_manager.is_bot() {
            panic!("CHECK failed: get_recently_opened_dialogs is not for bots");
        }
        let result = self.recently_opened_dialogs.get_dialogs(td, limit);
        if !self.recently_opened_dialogs.is_loaded && td.global.close_flag() {
            // the C++ load-promise abort (see `get_dialogs`)
            promise.set_error(Status::error(500, "Request aborted"));
            return (0, Vec::new());
        }
        promise.set_value(());
        result
    }

    /// `DialogManager::on_dialog_opened` (DialogManager.cpp:1798-1802).
    pub fn on_dialog_opened(&mut self, td: &mut Td, dialog_id: DialogId) {
        if !td.auth_manager.is_bot() {
            self.recently_opened_dialogs.add_dialog(td, dialog_id);
        }
    }

    /// `DialogManager::on_dialog_deleted` (DialogManager.cpp:1804-1809).
    pub fn on_dialog_deleted(&mut self, td: &mut Td, dialog_id: DialogId) {
        if !td.auth_manager.is_bot() {
            self.recently_found_dialogs.remove_dialog(td, dialog_id);
            self.recently_opened_dialogs.remove_dialog(td, dialog_id);
        }
    }
}

// The public M.6.17 entry points: free functions over `&mut Td` (the manager
// state is a plain Td field, so the call sites detach it for the call — the
// C++ `td_->dialog_manager_->` shape).

/// `DialogManager::add_dialog_to_hints`.
pub fn add_dialog_to_hints(td: &mut Td, dialog_id: DialogId) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    manager.add_dialog_to_hints(td, dialog_id);
    td.dialog_manager = manager;
}

/// `DialogManager::update_dialog_hints_rating`.
pub fn update_dialog_hints_rating(td: &mut Td, dialog_id: DialogId, rating: i64) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    manager.update_dialog_hints_rating(td, dialog_id, rating);
    td.dialog_manager = manager;
}

/// `DialogManager::search_dialogs` (DialogManager.cpp:1828-1860).
pub fn search_dialogs(
    td: &mut Td,
    query: &str,
    chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
    limit: i32,
    promise: Promise<()>,
) -> (i32, Vec<DialogId>) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    let result = manager.search_dialogs(td, query, chat_type_filter, limit, promise);
    td.dialog_manager = manager;
    result
}

/// `DialogManager::search_recently_found_dialogs` (DialogManager.cpp:1862-1890).
pub fn search_recently_found_dialogs(
    td: &mut Td,
    query: &str,
    chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
    limit: i32,
    promise: Promise<()>,
) -> (i32, Vec<DialogId>) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    let result = manager.search_recently_found_dialogs(td, query, chat_type_filter, limit, promise);
    td.dialog_manager = manager;
    result
}

/// `DialogManager::add_recently_found_dialog` (DialogManager.cpp:1892-1898).
pub fn add_recently_found_dialog(td: &mut Td, dialog_id: DialogId) -> Result<()> {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    let result = manager.add_recently_found_dialog(td, dialog_id);
    td.dialog_manager = manager;
    result
}

/// `DialogManager::remove_recently_found_dialog` (DialogManager.cpp:1900-1906).
pub fn remove_recently_found_dialog(td: &mut Td, dialog_id: DialogId) -> Result<()> {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    let result = manager.remove_recently_found_dialog(td, dialog_id);
    td.dialog_manager = manager;
    result
}

/// `DialogManager::clear_recently_found_dialogs` (DialogManager.cpp:1908-1910).
pub fn clear_recently_found_dialogs(td: &mut Td) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    manager.clear_recently_found_dialogs(td);
    td.dialog_manager = manager;
}

/// `DialogManager::get_recently_opened_dialogs` (DialogManager.cpp:1912-1915).
pub fn get_recently_opened_dialogs(
    td: &mut Td,
    limit: i32,
    promise: Promise<()>,
) -> (i32, Vec<DialogId>) {
    let mut manager = std::mem::take(&mut td.dialog_manager);
    let result = manager.get_recently_opened_dialogs(td, limit, promise);
    td.dialog_manager = manager;
    result
}

/// `DialogManager::get_dialog_type_filter` (DialogManager.cpp:3340-3355).
fn get_dialog_type_filter(
    type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
) -> DialogTypeFilter {
    use crate::td_api;
    let Some(type_filter) = type_filter else {
        return DialogTypeFilter::None;
    };
    match type_filter {
        td_api::SearchChatTypeFilter::searchChatTypeFilterBot(_) => DialogTypeFilter::Bot,
        td_api::SearchChatTypeFilter::searchChatTypeFilterChannel(_) => DialogTypeFilter::Broadcast,
    }
}

/// `DialogManager::is_dialog_suitable_for_type_filter` (DialogManager.cpp:3326-3339).
fn is_dialog_suitable_for_type_filter(
    td: &mut Td,
    dialog_id: DialogId,
    type_filter: DialogTypeFilter,
) -> bool {
    match type_filter {
        DialogTypeFilter::None => true,
        DialogTypeFilter::Bot => {
            dialog_id.get_type() == crate::dialog_id::DialogType::User
                && td.user_manager.is_user_bot_of(dialog_id.get_user_id())
        }
        DialogTypeFilter::Broadcast => is_broadcast_channel(td, dialog_id),
    }
}

/// `DialogManager::get_my_dialog_id` (DialogManager.h) — the dialog id of
/// the current user.
pub(crate) fn get_my_dialog_id(td: &Td) -> DialogId {
    DialogId::from_user_id(td.user_manager.get_my_id())
}

/// The C++ net-blocked promise completion — every server-query tail in this
/// slice ends here (the C++ promise completes from the query's
/// `on_result`/`on_error`, unreachable until the net step).
fn promise_unimplemented<T: Send>(promise: &mut Promise<T>) {
    promise.set_error(Status::error(500, "Network is not implemented yet"));
}

// M.6.18 — migrate + delete + protected content + report surface
// (DialogManager.cpp:1755-1810, :2030-2340, :2800-2960).

/// `DialogManager::migrate_dialog_to_megagroup` (DialogManager.cpp:1755-1781).
/// The creator gate reads the unloaded chat status (stub inventory), and the
/// `MigrateChatQuery` send is [blocked: net].
pub fn migrate_dialog_to_megagroup(
    td: &mut Td,
    dialog_id: DialogId,
    mut promise: Promise<crate::td_api::Object>,
) {
    if !have_dialog_force(td, dialog_id, "migrate_dialog_to_megagroup") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }
    if dialog_id.get_type() != crate::dialog_id::DialogType::Chat {
        promise.set_error(Status::error(
            400,
            "Only basic group chats can be converted to supergroup",
        ));
        return;
    }

    let _chat_id = dialog_id.get_chat_id();
    // get_chat_status(chat_id).is_creator() — the C++ has no CHECK here;
    // the unloaded-chat status is `Banned(0, "")` (ChatManager.cpp:8559-8562),
    // whose is_creator() is false (stub inventory). The gate answers the
    // same "not creator" decision.
    let is_creator =
        crate::chat_manager::DialogParticipantStatus::banned(0, String::new()).is_creator();
    if !is_creator {
        promise.set_error(Status::error(400, "Need creator rights in the chat"));
        return;
    }
    // get_chat_migrated_to_channel_id(chat_id).is_valid() — unloaded →
    // invalid → the C++ falls through to the query (stub inventory).
    tracing::info!("Migrate basic group to supergroup — [blocked: net]");
    promise_unimplemented(&mut promise);
}

/// `DialogManager::on_migrate_chat_to_megagroup`
/// (DialogManager.cpp:1782-1797) — the migrated-to channel id is the
/// unloaded invalid (stub inventory), so only the error head is reachable.
pub fn on_migrate_chat_to_megagroup(
    td: &mut Td,
    _chat_id: crate::chat_manager::ChatId,
    mut promise: Promise<crate::td_api::Object>,
) {
    let _ = td;
    // C++: !get_chat_migrated_to_channel_id(chat_id).is_valid()
    tracing::error!("Can't find the supergroup to which the basic group has migrated");
    promise.set_error(Status::error(500, "Supergroup not found"));
}

/// `DialogManager::delete_dialog` (DialogManager.cpp:2030-2050) — the
/// per-type cascade lands with MessagesManager/ChatManager/the
/// SecretChatsManager (stub inventory); every branch currently completes
/// with the net-blocked error.
pub fn delete_dialog(td: &mut Td, dialog_id: DialogId, mut promise: Promise<()>) {
    if !have_dialog_force(td, dialog_id, "delete_dialog") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::Channel => {
            // messages_manager_->delete_dialog_history /
            // chat_manager_->delete_chat / delete_channel (stub inventory).
            promise_unimplemented(&mut promise);
        }
        crate::dialog_id::DialogType::SecretChat => {
            // send_closure(secret_chats_manager_, cancel_chat) — the
            // SecretChatsManager is Wave 3 (stub inventory).
            promise_unimplemented(&mut promise);
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("delete_dialog on DialogType::None passed have_dialog_force")
        }
    }
}

/// `DialogManager::get_dialog_has_protected_content_force`
/// (DialogManager.cpp:2274-2290).
pub fn get_dialog_has_protected_content_force(td: &mut Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            // user_manager_->get_user_has_protected_content_force
            let Some(options) = td.option_manager.as_mut() else {
                return false;
            };
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .get_user_has_protected_content_force(&mut env, dialog_id.get_user_id())
        }
        // chat_manager_->get_chat_has_protected_content /
        // get_channel_has_protected_content — unloaded `false` (stub
        // inventory).
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
        crate::dialog_id::DialogType::SecretChat => false,
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_has_protected_content_force on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_has_protected_content`
/// (DialogManager.cpp:2291-2307).
pub fn get_dialog_has_protected_content(td: &Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_has_protected_content_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
        crate::dialog_id::DialogType::SecretChat => false,
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_has_protected_content on DialogType::None")
        }
    }
}

/// `DialogManager::is_dialog_action_unneeded` (DialogManager.cpp:2308-2339).
pub fn is_dialog_action_unneeded(td: &mut Td, dialog_id: DialogId) -> bool {
    if is_anonymous_administrator(td, dialog_id) {
        return true;
    }

    let dialog_type = dialog_id.get_type();
    if dialog_type == crate::dialog_id::DialogType::User
        || dialog_type == crate::dialog_id::DialogType::SecretChat
    {
        let user_id = if dialog_type == crate::dialog_id::DialogType::User {
            dialog_id.get_user_id()
        } else {
            td.user_manager
                .get_secret_chat_user_id(dialog_id.get_secret_chat_id())
        };
        if td.user_manager.is_user_deleted_of(user_id) {
            return true;
        }
        if td.user_manager.is_user_bot_of(user_id) && !td.user_manager.is_user_support_of(user_id) {
            return true;
        }
        if user_id == td.user_manager.get_my_id() {
            return true;
        }

        if !td.auth_manager.is_bot() && td.user_manager.is_user_status_exact_of(user_id) {
            // is_user_online(user_id, 30) — the Td clock feeds the env.
            let Some(options) = td.option_manager.as_mut() else {
                return false;
            };
            let env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            if !td.user_manager.is_user_online_of(&env, user_id, 30, 0) {
                return true;
            }
            // C++ else: `// return true;` — deliberately commented out
            // upstream; the inexact-status branch stays `false`.
        }
    }
    false
}

/// `DialogManager::load_dialog_marks_as_unread` (DialogManager.cpp:2800-2803)
/// — the `GetDialogUnreadMarksQuery` send is [blocked: net] (stub
/// inventory); until it can run, the pmc flag stays unset exactly like the
/// C++ before a successful fetch.
pub fn load_dialog_marks_as_unread(td: &mut Td) {
    if td
        .global
        .binlog_pmc()
        .get("fetched_marks_as_unread")
        .is_empty()
    {
        tracing::info!("Fetch dialog unread marks — [blocked: net]");
    }
}

/// `DialogManager::can_report_dialog` (DialogManager.cpp:2806-2823).
pub fn can_report_dialog(td: &mut Td, dialog_id: DialogId) -> bool {
    // doesn't include possibility of report from action bar
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            // user_manager_->can_report_user (UserManager.cpp:5479-5482):
            // `u != nullptr && !u->is_deleted && !u->is_support && u->is_bot`
            // — the C++ `get_user` performs no DB load and no special-user
            // synthesis, so the presence check is `have_min_user`.
            let user_id = dialog_id.get_user_id();
            td.user_manager.have_min_user(user_id)
                && !td.user_manager.is_user_deleted_of(user_id)
                && !td.user_manager.is_user_support_of(user_id)
                && td.user_manager.is_user_bot_of(user_id)
        }
        crate::dialog_id::DialogType::Chat => false,
        // !get_channel_status(channel_id).is_creator() — the unloaded
        // Banned status is not a creator (stub inventory).
        crate::dialog_id::DialogType::Channel => true,
        crate::dialog_id::DialogType::SecretChat => false,
        crate::dialog_id::DialogType::None => {
            unreachable!("can_report_dialog on DialogType::None")
        }
    }
}

/// `DialogManager::report_dialog` (DialogManager.cpp:2824-2851) — the
/// action-bar intake and the `ReportPeerQuery` send are stub inventory;
/// `can_report_message` is `Ok` (no messages exist).
pub fn report_dialog(
    td: &mut Td,
    dialog_id: DialogId,
    _option_id: &str,
    _message_ids: &[crate::chat_manager::MessageId],
    _text: &str,
    mut promise: Promise<crate::td_api::Object>,
) {
    let mut access = check_dialog_access(td, dialog_id, true, AccessRights::Read, "report_dialog");
    if access.is_error() {
        promise.set_error(access.move_as_error());
        return;
    }

    // messages_manager_->report_dialog_from_action_bar — {false, false}
    // (stub inventory): the C++ short-circuits only when the action bar
    // reported the dialog itself.
    if !can_report_dialog(td, dialog_id) {
        promise.set_error(Status::error(400, "Chat can't be reported"));
        return;
    }

    for _message_id in _message_ids {
        // messages_manager_->can_report_message — Ok (stub inventory).
    }

    // ReportPeerQuery send — [blocked: net] (stub inventory); the
    // MESSAGE_ID_REQUIRED on_error mapping lands with the net step.
    promise_unimplemented(&mut promise);
}

/// `DialogManager::report_dialog_photo` (DialogManager.cpp:2852-2875) — the
/// file-view gates need the FileManager ([blocked: files], stub inventory),
/// so the entry gates what it can and stops at the net-blocked query.
pub fn report_dialog_photo(
    td: &mut Td,
    dialog_id: DialogId,
    _file_id: crate::user_manager::FileId,
    mut promise: Promise<()>,
) {
    let mut access = check_dialog_access(
        td,
        dialog_id,
        false,
        AccessRights::Read,
        "report_dialog_photo",
    );
    if access.is_error() {
        promise.set_error(access.move_as_error());
        return;
    }

    if !can_report_dialog(td, dialog_id) {
        promise.set_error(Status::error(400, "Chat photo can't be reported"));
        return;
    }

    // file_manager_->get_file_view gates (empty/type/remote-photo) —
    // [blocked: files] (stub inventory); ReportProfilePhotoQuery —
    // [blocked: net] (stub inventory).
    promise_unimplemented(&mut promise);
}

/// `BlockListId` (BlockListId.h) — the block-list facet of
/// get_blocked_dialogs (1.8.67: Main and Stories).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockListId {
    Main,
    Stories,
}

impl BlockListId {
    /// `BlockListId(const td_api::object_ptr<td_api::BlockList> &)` —
    /// `None` is the invalid C++ state.
    pub fn from_block_list(block_list: Option<&crate::td_api::BlockList>) -> Option<BlockListId> {
        use crate::td_api;
        match block_list {
            None => None,
            Some(td_api::BlockList::blockListMain(_)) => Some(BlockListId::Main),
            Some(td_api::BlockList::blockListStories(_)) => Some(BlockListId::Stories),
        }
    }
}

/// `DialogManager::get_blocked_dialogs` (DialogManager.cpp:3583-3600) — the
/// `GetBlockedDialogsQuery` send is [blocked: net] (stub inventory).
pub fn get_blocked_dialogs(
    td: &mut Td,
    block_list: Option<&crate::td_api::BlockList>,
    offset: i32,
    limit: i32,
    mut promise: Promise<crate::td_api::Object>,
) {
    let _ = td;
    if offset < 0 {
        promise.set_error(Status::error(400, "Parameter offset must be non-negative"));
        return;
    }

    if limit <= 0 {
        promise.set_error(Status::error(400, "Parameter limit must be positive"));
        return;
    }

    if BlockListId::from_block_list(block_list).is_none() {
        promise.set_error(Status::error(400, "Block list must be non-empty"));
        return;
    }

    promise_unimplemented(&mut promise);
}

/// `UserManager::get_message_sender_object` shape
/// (UserManager.cpp get_message_sender_object) — the user half is wired;
/// the chat/channel halves answer the unloaded id-object form (stub
/// inventory).
pub(crate) fn get_message_sender_object(
    td: &mut Td,
    dialog_id: DialogId,
    source: &str,
) -> crate::td_api::MessageSender {
    use crate::td_api;
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let Some(options) = td.option_manager.as_mut() else {
                return td_api::MessageSender::messageSenderChat(td_api::messageSenderChat {
                    chat_id: dialog_id.get(),
                });
            };
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td_api::MessageSender::messageSenderUser(td_api::messageSenderUser {
                user_id: td.user_manager.get_user_id_object(
                    &mut env,
                    dialog_id.get_user_id(),
                    source,
                ),
            })
        }
        _ => td_api::MessageSender::messageSenderChat(td_api::messageSenderChat {
            chat_id: dialog_id.get(),
        }),
    }
}

/// `DialogManager::on_get_blocked_dialogs` (DialogManager.cpp:3601-3621) —
/// pure once the peers are fetched; reachable only from the net step.
pub fn on_get_blocked_dialogs(
    td: &mut Td,
    offset: i32,
    limit: i32,
    total_count: i32,
    blocked_peers: Vec<tdmtproto::telegram_api::PeerBlocked>,
    mut promise: Promise<crate::td_api::Object>,
) {
    let _ = limit;
    tracing::info!(
        "Receive {} blocked chats from offset {} out of {}",
        blocked_peers.len(),
        offset,
        total_count
    );
    let mut dialog_ids = Vec::new();
    for blocked_peer in blocked_peers {
        // get_message_sender_dialog_ids keeps user/chat/channel peers. Its
        // C++ filters — drop users without `have_min_user`, drop
        // chats/channels without `have_dialog_info`/`have_dialog`
        // (MessageSender.cpp:135-160) — are net-step residue: this
        // function is only fed by the blocked-dialogs query result, which
        // cannot arrive before the net step.
        let peer_dialog_id = match blocked_peer.peer_id {
            Some(tdmtproto::telegram_api::Peer::peerUser(peer)) => {
                DialogId::from_user_id(crate::user_manager::UserId::new(peer.user_id))
            }
            Some(tdmtproto::telegram_api::Peer::peerChat(peer)) => {
                DialogId::from_chat_id(crate::chat_manager::ChatId::new(peer.chat_id))
            }
            Some(tdmtproto::telegram_api::Peer::peerChannel(peer)) => {
                DialogId::from_channel_id(crate::chat_manager::ChannelId::new(peer.channel_id))
            }
            // the generated model's absent-peer case; the C++ dereference
            // cannot see it — skip the entry
            None => continue,
        };
        if peer_dialog_id.is_valid() {
            dialog_ids.push(peer_dialog_id);
        }
    }

    let mut total_count = total_count;
    // the C++ compares `offset + dialog_ids.size() >
    // static_cast<size_t>(total_count)` — the size_t cast means a negative
    // server count never triggers the fix
    if !dialog_ids.is_empty() && (offset + dialog_ids.len() as i32) as usize > total_count as usize
    {
        tracing::error!(
            "Fix total count of blocked chats from {} to {}",
            total_count,
            offset + dialog_ids.len() as i32
        );
        total_count = offset + dialog_ids.len() as i32;
    }

    let senders = dialog_ids
        .iter()
        .map(|&dialog_id| get_message_sender_object(td, dialog_id, "on_get_blocked_dialogs"))
        .collect();
    promise.set_value(crate::td_api::Object::messageSenders(
        crate::td_api::MessageSenders {
            total_count,
            senders,
        },
    ));
}

/// `DialogManager::reget_peer_settings` (DialogManager.cpp:3480-3485) — the
/// `GetPeerSettingsQuery` send is [blocked: net] (stub inventory).
/// Keep: no trigger yet — lands with the peer-settings update refire
/// (the C++ internal caller, DialogManager.cpp:609).
#[allow(dead_code)]
pub fn reget_peer_settings(td: &mut Td, dialog_id: DialogId) {
    if !have_input_peer(td, dialog_id, false, AccessRights::Read)
        || is_monoforum_channel(td, dialog_id)
    {
        return;
    }
    tracing::info!("Get peer settings — [blocked: net]");
}

// M.6.18 — the server-sync binlog mirrors (DialogManager.cpp:3491-3831).

/// `Version::AddFolders` (Version.h) — the value the log-event version must
/// reach before the reorder-pinned mirror carries a folder id.
pub(crate) const LOG_EVENT_ADD_FOLDERS_VERSION: i32 = 23;

/// `DialogManager::ReorderPinnedDialogsOnServerLogEvent`
/// (DialogManager.cpp:3488-3522): `FolderId` + `vector<DialogId>`, the
/// folder gated on `Version::AddFolders`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReorderPinnedDialogsOnServerLogEvent {
    pub folder_id: i32,
    pub dialog_ids: Vec<DialogId>,
}

impl ReorderPinnedDialogsOnServerLogEvent {
    fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        crate::store_parse::store_i32(out, self.folder_id);
        crate::store_parse::store_i32(out, self.dialog_ids.len() as i32);
        for &dialog_id in &self.dialog_ids {
            crate::store_parse::store_i64(out, dialog_id.get());
        }
    }

    fn parse(
        data: &[u8],
    ) -> std::result::Result<ReorderPinnedDialogsOnServerLogEvent, &'static str> {
        let mut parser = crate::store_parse::Parser::new_log_event(data)?;
        let folder_id = if parser.version() >= LOG_EVENT_ADD_FOLDERS_VERSION {
            parser.i32_()?
        } else {
            0 // FolderId()
        };
        let count = parser.i32_()?;
        if count < 0 || count as usize > parser.left_len() / 8 {
            return Err("Invalid dialog id count in ReorderPinnedDialogsOnServerLogEvent");
        }
        let mut dialog_ids = Vec::with_capacity(count as usize);
        for _ in 0..count {
            dialog_ids.push(DialogId::new(parser.i64_()?));
        }
        parser.fetch_end()?;
        Ok(ReorderPinnedDialogsOnServerLogEvent {
            folder_id,
            dialog_ids,
        })
    }
}

/// `DialogManager::ToggleDialogReportSpamStateOnServerLogEvent`
/// (DialogManager.cpp:3527-3565): `DialogId` + `is_spam_dialog`, no flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToggleDialogReportSpamStateOnServerLogEvent {
    pub dialog_id: DialogId,
    pub is_spam_dialog: bool,
}

impl ToggleDialogReportSpamStateOnServerLogEvent {
    fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        crate::store_parse::store_i64(out, self.dialog_id.get());
        crate::store_parse::store_bool(out, self.is_spam_dialog);
    }

    fn parse(
        data: &[u8],
    ) -> std::result::Result<ToggleDialogReportSpamStateOnServerLogEvent, &'static str> {
        let mut parser = crate::store_parse::Parser::new_log_event(data)?;
        let dialog_id = DialogId::new(parser.i64_()?);
        let is_spam_dialog = parser.bool()?;
        parser.fetch_end()?;
        Ok(ToggleDialogReportSpamStateOnServerLogEvent {
            dialog_id,
            is_spam_dialog,
        })
    }
}

/// `DialogManager::ToggleDialogIsBlockedOnServerLogEvent`
/// (DialogManager.cpp:3647-3673): the two flags, then the dialog id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToggleDialogIsBlockedOnServerLogEvent {
    pub dialog_id: DialogId,
    pub is_blocked: bool,
    pub is_blocked_for_stories: bool,
}

impl ToggleDialogIsBlockedOnServerLogEvent {
    fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        crate::store_parse::store_flags(out, [self.is_blocked, self.is_blocked_for_stories]);
        crate::store_parse::store_i64(out, self.dialog_id.get());
    }

    fn parse(
        data: &[u8],
    ) -> std::result::Result<ToggleDialogIsBlockedOnServerLogEvent, &'static str> {
        let mut parser = crate::store_parse::Parser::new_log_event(data)?;
        let [is_blocked, is_blocked_for_stories] = parser.parse_flags()?;
        let dialog_id = DialogId::new(parser.i64_()?);
        parser.fetch_end()?;
        Ok(ToggleDialogIsBlockedOnServerLogEvent {
            dialog_id,
            is_blocked,
            is_blocked_for_stories,
        })
    }
}

/// `DialogManager::SavedMessagesTopicId` stand-in (the C++ wraps an int64;
/// the full type arrives with MessagesManager) — stored raw.
pub(crate) type SavedMessagesTopicId = i64;

/// `DialogManager::ToggleDialogTopicPropertyOnServerLogEvent`
/// (DialogManager.cpp:3691-3724): the value flag + the optional topic id,
/// then the dialog id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToggleDialogTopicPropertyOnServerLogEvent {
    pub dialog_id: DialogId,
    pub saved_messages_topic_id: SavedMessagesTopicId,
    pub value: bool,
}

impl ToggleDialogTopicPropertyOnServerLogEvent {
    fn store(&self, out: &mut Vec<u8>) {
        let has_saved_messages_topic_id = self.saved_messages_topic_id != 0;
        crate::store_parse::store_log_event_version(out);
        crate::store_parse::store_flags(out, [self.value, has_saved_messages_topic_id]);
        crate::store_parse::store_i64(out, self.dialog_id.get());
        if has_saved_messages_topic_id {
            crate::store_parse::store_i64(out, self.saved_messages_topic_id);
        }
    }

    fn parse(
        data: &[u8],
    ) -> std::result::Result<ToggleDialogTopicPropertyOnServerLogEvent, &'static str> {
        let mut parser = crate::store_parse::Parser::new_log_event(data)?;
        let [value, has_saved_messages_topic_id] = parser.parse_flags()?;
        let dialog_id = DialogId::new(parser.i64_()?);
        let saved_messages_topic_id = if has_saved_messages_topic_id {
            parser.i64_()?
        } else {
            0
        };
        parser.fetch_end()?;
        Ok(ToggleDialogTopicPropertyOnServerLogEvent {
            dialog_id,
            saved_messages_topic_id,
            value,
        })
    }
}

/// `DialogManager::ToggleDialogPropertyOnServerLogEvent`
/// (DialogManager.cpp:3750-3772): the value flag, then the dialog id —
/// shared by the pinned/translatable/view-as-messages mirrors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToggleDialogPropertyOnServerLogEvent {
    pub dialog_id: DialogId,
    pub value: bool,
}

impl ToggleDialogPropertyOnServerLogEvent {
    fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        crate::store_parse::store_flags(out, [self.value]);
        crate::store_parse::store_i64(out, self.dialog_id.get());
    }

    fn parse(
        data: &[u8],
    ) -> std::result::Result<ToggleDialogPropertyOnServerLogEvent, &'static str> {
        let mut parser = crate::store_parse::Parser::new_log_event(data)?;
        let [value] = parser.parse_flags()?;
        let dialog_id = DialogId::new(parser.i64_()?);
        parser.fetch_end()?;
        Ok(ToggleDialogPropertyOnServerLogEvent { dialog_id, value })
    }
}

/// `DialogManager::save_reorder_pinned_dialogs_on_server_log_event`
/// (DialogManager.cpp:3524-3530).
pub(crate) fn save_reorder_pinned_dialogs_on_server_log_event(
    td: &mut Td,
    folder_id: i32,
    dialog_ids: &[DialogId],
) -> u64 {
    let log_event = ReorderPinnedDialogsOnServerLogEvent {
        folder_id,
        dialog_ids: dialog_ids.to_vec(),
    };
    let mut data = Vec::new();
    log_event.store(&mut data);
    td.global.td_db().binlog().add(
        tddb::td_db::LOG_EVENT_HANDLER_TYPE_REORDER_PINNED_DIALOGS_ON_SERVER,
        &data,
    )
}

/// `DialogManager::save_toggle_dialog_report_spam_state_on_server_log_event`
/// (DialogManager.cpp:3567-3575).
pub(crate) fn save_toggle_dialog_report_spam_state_on_server_log_event(
    td: &mut Td,
    dialog_id: DialogId,
    is_spam_dialog: bool,
) -> u64 {
    let log_event = ToggleDialogReportSpamStateOnServerLogEvent {
        dialog_id,
        is_spam_dialog,
    };
    let mut data = Vec::new();
    log_event.store(&mut data);
    td.global.td_db().binlog().add(
        tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_REPORT_SPAM_STATE_ON_SERVER,
        &data,
    )
}

/// `DialogManager::save_toggle_dialog_is_blocked_on_server_log_event`
/// (DialogManager.cpp:3674-3681).
pub(crate) fn save_toggle_dialog_is_blocked_on_server_log_event(
    td: &mut Td,
    dialog_id: DialogId,
    is_blocked: bool,
    is_blocked_for_stories: bool,
) -> u64 {
    let log_event = ToggleDialogIsBlockedOnServerLogEvent {
        dialog_id,
        is_blocked,
        is_blocked_for_stories,
    };
    let mut data = Vec::new();
    log_event.store(&mut data);
    td.global.td_db().binlog().add(
        tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_BLOCKED_ON_SERVER,
        &data,
    )
}

/// `DialogManager::save_toggle_dialog_is_marked_as_unread_on_server_log_event`
/// (DialogManager.cpp:3726-3733).
pub(crate) fn save_toggle_dialog_is_marked_as_unread_on_server_log_event(
    td: &mut Td,
    dialog_id: DialogId,
    saved_messages_topic_id: SavedMessagesTopicId,
    is_marked_as_unread: bool,
) -> u64 {
    let log_event = ToggleDialogTopicPropertyOnServerLogEvent {
        dialog_id,
        saved_messages_topic_id,
        value: is_marked_as_unread,
    };
    let mut data = Vec::new();
    log_event.store(&mut data);
    td.global.td_db().binlog().add(
        tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_MARKED_AS_UNREAD_ON_SERVER,
        &data,
    )
}

/// `DialogManager::save_toggle_dialog_{is_pinned,is_translatable,view_as_messages}_on_server_log_event`
/// — one record shape (DialogManager.cpp:3788-3830).
pub(crate) fn save_toggle_dialog_property_on_server_log_event(
    td: &mut Td,
    handler_type: i32,
    dialog_id: DialogId,
    value: bool,
) -> u64 {
    let log_event = ToggleDialogPropertyOnServerLogEvent { dialog_id, value };
    let mut data = Vec::new();
    log_event.store(&mut data);
    td.global.td_db().binlog().add(handler_type, &data)
}

/// `DialogManager::reorder_pinned_dialogs_on_server`
/// (DialogManager.cpp:3517-3524) — the mirror save honors
/// `use_message_database`; the `ReorderPinnedDialogsQuery` send is
/// [blocked: net] (stub inventory). The C++ erase-on-completion promise
/// (`get_erase_log_event_promise`) erases the mirror on settlement —
/// success or error — while the Rust mirror stays until the net step can
/// complete it (net-step residue).
pub fn reorder_pinned_dialogs_on_server(
    td: &mut Td,
    folder_id: i32,
    dialog_ids: &[DialogId],
    log_event_id: u64,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_reorder_pinned_dialogs_on_server_log_event(td, folder_id, dialog_ids);
    }
    tracing::info!("Reorder pinned dialogs on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::toggle_dialog_report_spam_state_on_server`
/// (DialogManager.cpp:3552-3582). The `input_peer == nullptr` shortcut of
/// `UpdatePeerSettingsQuery::send` (cpp:652-654) is a net-free SUCCESS
/// path: with the chat/channel input peers stubbed to `None` (stub
/// inventory), every chat/channel dialog completes `Unit()` exactly like
/// the C++ stub world.
pub fn toggle_dialog_report_spam_state_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    is_spam_dialog: bool,
    log_event_id: u64,
    mut promise: Promise<()>,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id =
            save_toggle_dialog_report_spam_state_on_server_log_event(td, dialog_id, is_spam_dialog);
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::Channel => {
            if get_input_peer(td, dialog_id, AccessRights::Read).is_none() {
                // UpdatePeerSettingsQuery::send, cpp:652-654
                promise.set_value(());
                return log_event_id;
            }
            // UpdatePeerSettingsQuery send — [blocked: net] (stub inventory).
        }
        crate::dialog_id::DialogType::SecretChat => {
            if is_spam_dialog {
                // ReportEncryptedSpamQuery send — [blocked: net].
            } else {
                let user_id = td
                    .user_manager
                    .get_secret_chat_user_id(dialog_id.get_secret_chat_id());
                if !user_id.is_valid() {
                    promise.set_error(Status::error(400, "Peer user not found"));
                    return log_event_id;
                }
                // UpdatePeerSettingsQuery(DialogId(user_id), false) —
                // [blocked: net].
            }
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("toggle_dialog_report_spam_state_on_server on DialogType::None")
        }
    }
    promise_unimplemented(&mut promise);
    log_event_id
}

/// `DialogManager::toggle_dialog_is_blocked_on_server`
/// (DialogManager.cpp:3681-3690).
pub fn toggle_dialog_is_blocked_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    is_blocked: bool,
    is_blocked_for_stories: bool,
    log_event_id: u64,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_toggle_dialog_is_blocked_on_server_log_event(
            td,
            dialog_id,
            is_blocked,
            is_blocked_for_stories,
        );
    }
    // ToggleDialogIsBlockedQuery send — [blocked: net] (stub inventory).
    tracing::info!("Toggle dialog blocked state on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::toggle_dialog_is_marked_as_unread_on_server`
/// (DialogManager.cpp:3733-3748) — secret chats never even create the
/// binlog event.
pub fn toggle_dialog_is_marked_as_unread_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    saved_messages_topic_id: SavedMessagesTopicId,
    is_marked_as_unread: bool,
    log_event_id: u64,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat {
        // don't even create new binlog events
        return 0;
    }

    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_toggle_dialog_is_marked_as_unread_on_server_log_event(
            td,
            dialog_id,
            saved_messages_topic_id,
            is_marked_as_unread,
        );
    }
    // ToggleDialogUnreadMarkQuery send — [blocked: net] (stub inventory).
    tracing::info!("Toggle dialog unread mark on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::toggle_dialog_is_pinned_on_server`
/// (DialogManager.cpp:3780-3788).
pub fn toggle_dialog_is_pinned_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    is_pinned: bool,
    log_event_id: u64,
) -> u64 {
    // C++ `CHECK(!td_->auth_manager_->is_bot())`
    if td.auth_manager.is_bot() {
        panic!("CHECK failed: toggle_dialog_is_pinned_on_server is not for bots");
    }
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat {
        // don't even create new binlog events
        return 0;
    }

    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_toggle_dialog_property_on_server_log_event(
            td,
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_PINNED_ON_SERVER,
            dialog_id,
            is_pinned,
        );
    }
    // ToggleDialogPinQuery send — [blocked: net] (stub inventory).
    tracing::info!("Toggle dialog pin on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::toggle_dialog_is_translatable_on_server`
/// (DialogManager.cpp:3800-3810).
pub fn toggle_dialog_is_translatable_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    is_translatable: bool,
    log_event_id: u64,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat {
        // don't even create new binlog events
        return 0;
    }

    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_toggle_dialog_property_on_server_log_event(
            td,
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_TRANSLATABLE_ON_SERVER,
            dialog_id,
            is_translatable,
        );
    }
    // ToggleDialogTranslationsQuery send — [blocked: net] (stub inventory).
    tracing::info!("Toggle dialog translatable on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::toggle_dialog_view_as_messages_on_server`
/// (DialogManager.cpp:3822-3831).
pub fn toggle_dialog_view_as_messages_on_server(
    td: &mut Td,
    dialog_id: DialogId,
    view_as_messages: bool,
    log_event_id: u64,
) -> u64 {
    let mut log_event_id = log_event_id;
    if log_event_id == 0 && td.global.td_db().parameters().use_message_database {
        log_event_id = save_toggle_dialog_property_on_server_log_event(
            td,
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_VIEW_AS_MESSAGES_ON_SERVER,
            dialog_id,
            view_as_messages,
        );
    }
    // ToggleViewForumAsMessagesQuery send — [blocked: net] (stub inventory).
    tracing::info!("Toggle dialog view-as-messages on server — [blocked: net]");
    log_event_id
}

/// `DialogManager::set_dialog_default_send_as_on_server`
/// (DialogManager.cpp:3628-3632). Keep: zero callers — the td_api arm is
/// a placeholder that never calls it; this is the correct net tail and
/// lands with the dialog net slice (§3.4).
#[allow(dead_code)]
pub fn set_dialog_default_send_as_on_server(
    _td: &mut Td,
    _dialog_id: DialogId,
    _send_as_dialog_id: DialogId,
    mut promise: Promise<()>,
) {
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_folder_id_on_server`
/// (DialogManager.cpp:3633-3637). Keep: zero callers — the td_api arm is
/// a placeholder that never calls it; this is the correct net tail and
/// lands with the dialog net slice (§3.4).
#[allow(dead_code)]
pub fn set_dialog_folder_id_on_server(
    _td: &mut Td,
    _dialog_id: DialogId,
    _folder_id: i32,
    mut promise: Promise<()>,
) {
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_message_ttl_on_server`
/// (DialogManager.cpp:3638-3641). Keep: zero callers — the td_api arm is
/// a placeholder that never calls it; this is the correct net tail and
/// lands with the dialog net slice (§3.4).
#[allow(dead_code)]
pub fn set_dialog_message_ttl_on_server(
    _td: &mut Td,
    _dialog_id: DialogId,
    _ttl: i32,
    mut promise: Promise<()>,
) {
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_theme_on_server` (DialogManager.cpp:3642-3646).
pub fn set_dialog_theme_on_server(
    _td: &mut Td,
    _dialog_id: DialogId,
    _theme_name: &str,
    _is_gift: bool,
    mut promise: Promise<()>,
) {
    promise_unimplemented(&mut promise);
}

/// `DialogManager::on_binlog_events` (DialogManager.cpp:3832-3989) — the
/// replay arm for the seven server-sync mirrors. `have_old_message_database`
/// is the C++ `use_message_database() && !was_dialog_db_created()` pair; the
/// refired server sends are [blocked: net], so a replayed mirror either
/// erases (the C++ fresh-DB / gate-failure branches) or re-saves nothing and
/// stays pending exactly like the C++ with no network.
pub fn on_binlog_dialog_events(td: &mut Td, events: &[tddb::binlog::event::BinlogEvent]) {
    if td.global.close_flag() {
        return;
    }
    let have_old_message_database = td.global.td_db().parameters().use_message_database
        && !td.global.td_db().was_dialog_db_created();
    for event in events {
        assert!(event.id != 0);
        match event.type_ {
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_REORDER_PINNED_DIALOGS_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) = ReorderPinnedDialogsOnServerLogEvent::parse(event.get_data())
                else {
                    continue; // log_event_parse(...).ensure() aborts upstream; a
                    // bad event here is a foreign binlog
                };
                let dialog_ids: Vec<DialogId> = log_event
                    .dialog_ids
                    .iter()
                    .copied()
                    .filter(|&dialog_id| {
                        have_dialog_force(td, dialog_id, "ReorderPinnedDialogsOnServerLogEvent")
                            && have_input_peer(td, dialog_id, true, AccessRights::Read)
                    })
                    .collect();
                if dialog_ids.is_empty() {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                reorder_pinned_dialogs_on_server(td, log_event.folder_id, &dialog_ids, event.id);
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_BLOCKED_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) = ToggleDialogIsBlockedOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                let dialog_id = log_event.dialog_id;
                if dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
                    || !have_dialog_info_force(td, dialog_id, "ToggleDialogIsBlockedOnServer")
                    || !have_input_peer(td, dialog_id, true, AccessRights::Know)
                {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                toggle_dialog_is_blocked_on_server(
                    td,
                    dialog_id,
                    log_event.is_blocked,
                    log_event.is_blocked_for_stories,
                    event.id,
                );
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_MARKED_AS_UNREAD_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) =
                    ToggleDialogTopicPropertyOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                // dependencies.resolve_force — the topic/dialog load machinery
                // is MessagesManager-side; the C++ erases the event when the
                // dependency cannot resolve, which is every case here
                // (stub inventory). The parse above is the C++
                // log_event_parse(...).ensure() gate.
                let _ = log_event;
                td.global.td_db().binlog().erase(event.id);
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_PINNED_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) = ToggleDialogPropertyOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                let dialog_id = log_event.dialog_id;
                if !have_dialog_force(td, dialog_id, "ToggleDialogIsPinnedOnServer")
                    || !have_input_peer(td, dialog_id, true, AccessRights::Read)
                {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                toggle_dialog_is_pinned_on_server(td, dialog_id, log_event.value, event.id);
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_TRANSLATABLE_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) = ToggleDialogPropertyOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                let dialog_id = log_event.dialog_id;
                if !have_dialog_force(td, dialog_id, "ToggleDialogIsTranslatableOnServer")
                    || !have_input_peer(td, dialog_id, true, AccessRights::Read)
                {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                toggle_dialog_is_translatable_on_server(td, dialog_id, log_event.value, event.id);
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_REPORT_SPAM_STATE_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) =
                    ToggleDialogReportSpamStateOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                let dialog_id = log_event.dialog_id;
                if !have_dialog_force(td, dialog_id, "ToggleDialogReportSpamStateOnServer")
                    || !have_input_peer(td, dialog_id, true, AccessRights::Read)
                {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                toggle_dialog_report_spam_state_on_server(
                    td,
                    dialog_id,
                    log_event.is_spam_dialog,
                    event.id,
                    Promise::default(),
                );
            }
            tddb::td_db::LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_VIEW_AS_MESSAGES_ON_SERVER => {
                if !have_old_message_database {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                let Ok(log_event) = ToggleDialogPropertyOnServerLogEvent::parse(event.get_data())
                else {
                    continue;
                };
                let dialog_id = log_event.dialog_id;
                if !have_dialog_force(td, dialog_id, "ToggleDialogViewAsMessagesOnServer")
                    || !have_input_peer(td, dialog_id, true, AccessRights::Read)
                {
                    td.global.td_db().binlog().erase(event.id);
                    continue;
                }
                toggle_dialog_view_as_messages_on_server(td, dialog_id, log_event.value, event.id);
            }
            other => {
                unreachable!("Unsupported dialog log event type {other:#x}")
            }
        }
    }
}

// M.6.19 — the dialog edit surface (DialogManager.cpp:2341-2800) and the
// username resolve family (:3018-3549).

/// `DialogManager::set_dialog_title` (DialogManager.cpp:2341-2384). The
/// Chat/Channel rights gates answer the unloaded defaults (stub inventory),
/// so the C++ tail (the no-op check against `get_dialog_title` + the
/// `EditDialogTitleQuery` send) is ported but unreachable until the
/// ChatManager read surface lands.
#[allow(unreachable_code, unused_variables)]
pub fn set_dialog_title(td: &mut Td, dialog_id: DialogId, title: &str, mut promise: Promise<()>) {
    if !have_dialog_force(td, dialog_id, "set_dialog_title") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    let new_title = crate::misc::clean_name(title, MAX_TITLE_LENGTH);
    if new_title.is_empty() {
        promise.set_error(Status::error(400, "Title must be non-empty"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            promise.set_error(Status::error(400, "Can't change private chat title"));
            return;
        }
        crate::dialog_id::DialogType::Chat => {
            // get_chat_permissions — the unloaded default cannot change info
            // (stub inventory); the is_appointed_chat_administrator bot
            // conjunct is unreachable past the same gate.
            promise.set_error(Status::error(400, "Not enough rights to change chat title"));
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            // get_channel_permissions — the unloaded default (stub inventory).
            promise.set_error(Status::error(400, "Not enough rights to change chat title"));
            return;
        }
        crate::dialog_id::DialogType::SecretChat => {
            promise.set_error(Status::error(400, "Can't change secret chat title"));
            return;
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("set_dialog_title on DialogType::None passed have_dialog_force")
        }
    }

    // TODO this can be wrong if there were previous change title requests
    // (the C++ comment).
    if get_dialog_title(td, dialog_id) == new_title {
        promise.set_value(());
        return;
    }
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_photo` (DialogManager.cpp:2385-2412, the
/// fb05f53c2 shape) — the dialog-type gates, then the
/// `do_set_dialog_photo` ladder. The FileManager upload halves are
/// [blocked: files] and the `EditDialogPhotoQuery` send is [blocked: net]
/// (stub inventory); for a non-regular channel (a community) the arm
/// answers "Can't change chat photo" after the rights gate. The
/// Chat/Channel rights gates answer the unloaded defaults, so the
/// is_regular_channel dispatch is unreachable until the ChatManager read
/// surface lands.
#[allow(unreachable_code, unused_variables)]
pub fn set_dialog_photo(
    td: &mut Td,
    dialog_id: DialogId,
    input_photo: Option<&crate::td_api::InputChatPhoto>,
    mut promise: Promise<()>,
) {
    if !have_dialog_force(td, dialog_id, "set_dialog_photo") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            promise.set_error(Status::error(400, "Can't change private chat photo"));
            return;
        }
        crate::dialog_id::DialogType::Chat => {
            // get_chat_permissions — the unloaded default (stub
            // inventory).
            promise.set_error(Status::error(400, "Not enough rights to change chat photo"));
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            // get_channel_permissions — the unloaded default (stub
            // inventory).
            promise.set_error(Status::error(400, "Not enough rights to change chat photo"));
            return;
            // `if (!channel_id.is_regular_channel())` — "Can't change chat
            // photo" (fb05f53c2); unreachable over the unloaded rights.
        }
        crate::dialog_id::DialogType::SecretChat => {
            promise.set_error(Status::error(400, "Can't change secret chat photo"));
            return;
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("set_dialog_photo on DialogType::None passed have_dialog_force")
        }
    }

    do_set_dialog_photo(td, dialog_id, dialog_id, input_photo, promise);
}

/// `DialogManager::do_set_dialog_photo` (6ccfd7660,
/// DialogManager.cpp:2414-2497) — the input-photo shape ladder shared by
/// the request plane and the community photo flow; `owner_dialog_id` is
/// the file-upload owner (the community's fake dialog id carries the
/// DialogId() owner). The upload/query tails park at the [blocked:
/// files]/[blocked: net] seams (stub inventory).
#[allow(unreachable_code, unused_variables)]
pub fn do_set_dialog_photo(
    td: &mut Td,
    dialog_id: DialogId,
    owner_dialog_id: DialogId,
    input_photo: Option<&crate::td_api::InputChatPhoto>,
    mut promise: Promise<()>,
) {
    use crate::td_api;
    let _ = (td, owner_dialog_id);

    let mut input_file_present = false;
    let mut main_frame_timestamp = 0.0;
    let mut is_animation = false;
    if let Some(input_photo) = input_photo {
        match input_photo {
            td_api::InputChatPhoto::inputChatPhotoPrevious(photo) => {
                // get_profile_photo_file_id + the main-remote-location file
                // view — [blocked: files] (stub inventory); the C++ answers
                // 400 on an unknown/invalid photo id before touching the
                // file view.
                let _ = photo.chat_photo_id;
                promise.set_error(Status::error(
                    400,
                    "Unknown profile photo identifier specified",
                ));
                return;
            }
            td_api::InputChatPhoto::inputChatPhotoStatic(_) => {
                // photo:InputFile — the union field is always present; the
                // C++ keeps the address for get_input_file_id.
                input_file_present = true;
            }
            td_api::InputChatPhoto::inputChatPhotoAnimation(photo) => {
                input_file_present = true;
                main_frame_timestamp = photo.main_frame_timestamp;
                is_animation = true;
            }
            td_api::InputChatPhoto::inputChatPhotoSticker(_) => {
                // StickerPhotoSize::get_sticker_photo_size needs the
                // sticker read surface (M.8/M.9 residue); the C++ built an
                // inputChatUploadedPhoto with VIDEO_EMOJI_MARKUP and sent
                // the edit query — [blocked: net] (stub inventory).
                promise_unimplemented(&mut promise);
                return;
            }
        }
    }

    if !input_file_present {
        // send_edit_dialog_photo_query(inputChatPhotoEmpty) — [blocked: net].
        promise_unimplemented(&mut promise);
        return;
    }

    const MAX_ANIMATION_DURATION: f64 = 10.0;
    if !(0.0..=MAX_ANIMATION_DURATION).contains(&main_frame_timestamp) {
        promise.set_error(Status::error(400, "Wrong main frame timestamp specified"));
        return;
    }

    // get_input_file_id + upload_dialog_photo — [blocked: files] (stub
    // inventory).
    let _ = is_animation;
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_accent_color` (DialogManager.cpp:2594-2619).
pub fn set_dialog_accent_color(
    td: &mut Td,
    dialog_id: DialogId,
    accent_color_id: crate::peer_color::AccentColorId,
    background_custom_emoji_id: crate::emoji_status::CustomEmojiId,
    mut promise: Promise<()>,
) {
    if !have_dialog_force(td, dialog_id, "set_dialog_accent_color") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User if dialog_id == get_my_dialog_id(td) => {
            // user_manager_->set_accent_color — the local half; the
            // account.updateColor send is [blocked: net].
            match td
                .user_manager
                .set_accent_color_local(accent_color_id, background_custom_emoji_id)
            {
                Ok(_) => promise_unimplemented(&mut promise),
                Err(status) => promise.set_error(status),
            }
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            // chat_manager_->set_channel_accent_color (ChatManager.cpp:3275)
            // — the flow dispatches the `UpdateChannelColorQuery` over the
            // ChatManager net handle.
            let crate::td::Td {
                global,
                option_manager,
                auth_manager,
                user_manager,
                chat_manager,
                ..
            } = td;
            let Some(options) = option_manager.as_mut() else {
                promise_unimplemented(&mut promise);
                return;
            };
            let mut env = UserEnv {
                global,
                options,
                auth_is_bot: auth_manager.is_bot(),
            };
            chat_manager.set_channel_accent_color(
                user_manager,
                &mut env,
                dialog_id.get_channel_id(),
                accent_color_id,
                background_custom_emoji_id,
                promise,
            );
            return;
        }
        _ => {}
    }
    promise.set_error(Status::error(400, "Can't change accent color in the chat"));
}

/// `DialogManager::set_dialog_profile_accent_color`
/// (DialogManager.cpp:2620-2647).
pub fn set_dialog_profile_accent_color(
    td: &mut Td,
    dialog_id: DialogId,
    profile_accent_color_id: crate::peer_color::AccentColorId,
    profile_background_custom_emoji_id: crate::emoji_status::CustomEmojiId,
    mut promise: Promise<()>,
) {
    if !have_dialog_force(td, dialog_id, "set_dialog_profile_accent_color") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User if dialog_id == get_my_dialog_id(td) => {
            // user_manager_->set_profile_accent_color — its local half
            // arrives with the profile-colors step; the query tail is
            // the net-blocked error (stub inventory).
            let _ = (profile_accent_color_id, profile_background_custom_emoji_id);
            promise_unimplemented(&mut promise);
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            // chat_manager_->set_channel_profile_accent_color
            // (ChatManager.cpp:3296) — the profile variant of the
            // `UpdateChannelColorQuery` flow.
            let crate::td::Td {
                global,
                option_manager,
                auth_manager,
                user_manager,
                chat_manager,
                ..
            } = td;
            let Some(options) = option_manager.as_mut() else {
                promise_unimplemented(&mut promise);
                return;
            };
            let mut env = UserEnv {
                global,
                options,
                auth_is_bot: auth_manager.is_bot(),
            };
            chat_manager.set_channel_profile_accent_color(
                user_manager,
                &mut env,
                dialog_id.get_channel_id(),
                profile_accent_color_id,
                profile_background_custom_emoji_id,
                promise,
            );
            return;
        }
        _ => {}
    }
    promise.set_error(Status::error(
        400,
        "Can't change profile accent color in the chat",
    ));
}

/// `DialogManager::set_dialog_permissions` (DialogManager.cpp:2648-2695).
/// The Chat/Channel rights gates answer the unloaded defaults (stub
/// inventory), so the C++ tail (the equal-permissions no-op check against
/// `get_dialog_default_permissions` + the `EditChatDefaultBannedRightsQuery`
/// send) is ported but unreachable until the ChatManager read surface lands.
#[allow(unreachable_code, unused_variables, unused_mut)]
pub fn set_dialog_permissions(
    td: &mut Td,
    dialog_id: DialogId,
    permissions: Option<&crate::td_api::ChatPermissions>,
    mut promise: Promise<()>,
) {
    let mut access = check_dialog_access(
        td,
        dialog_id,
        false,
        AccessRights::Write,
        "set_dialog_permissions",
    );
    if access.is_error() {
        promise.set_error(access.move_as_error());
        return;
    }

    let Some(permissions) = permissions else {
        promise.set_error(Status::error(400, "New permissions must be non-empty"));
        return;
    };

    let mut channel_type = ChannelType::Unknown;
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            promise.set_error(Status::error(400, "Can't change private chat permissions"));
            return;
        }
        crate::dialog_id::DialogType::Chat => {
            // get_chat_permissions — the unloaded default cannot restrict
            // (stub inventory).
            promise.set_error(Status::error(
                400,
                "Not enough rights to change chat permissions",
            ));
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            if is_broadcast_channel(td, dialog_id) {
                promise.set_error(Status::error(400, "Can't change channel chat permissions"));
                return;
            }
            // get_channel_permissions — the unloaded default (stub
            // inventory); the C++ proceeds with ChannelType::Megagroup.
            promise.set_error(Status::error(
                400,
                "Not enough rights to change chat permissions",
            ));
            return;
        }
        crate::dialog_id::DialogType::SecretChat | crate::dialog_id::DialogType::None => {
            unreachable!("set_dialog_permissions on a non-group dialog")
        }
    }

    // The equal-permissions no-op check compares against
    // [`get_dialog_default_permissions`] (the chat/channel halves answer the
    // unloaded `restrict_all()`, stub inventory); the
    // EditChatDefaultBannedRightsQuery send is [blocked: net].
    {
        let _new_permissions = crate::chat_manager::RestrictedRights::from_chat_permissions(
            Some(permissions),
            channel_type,
        );
        promise_unimplemented(&mut promise);
    }
}

/// `DialogManager::set_dialog_emoji_status` (DialogManager.cpp:2696-2720).
/// The td_api → `EmojiStatus` conversion is M.8/M.9 residue, so the entry
/// takes the value type directly (the C++ `unique_ptr<EmojiStatus>`). The
/// C++ mutates the local user only in the `UpdateEmojiStatusQuery` SUCCESS
/// callback (`on_set_emoji_status`, UserManager.cpp:6765-6773); with the
/// query net-blocked the port runs only the premium gate and fails the
/// promise, leaving `my_user` untouched — like every other failed call.
pub fn set_dialog_emoji_status(
    td: &mut Td,
    dialog_id: DialogId,
    emoji_status: Option<&crate::emoji_status::EmojiStatus>,
    mut promise: Promise<()>,
) {
    if !have_dialog_force(td, dialog_id, "set_dialog_emoji_status") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User if dialog_id == get_my_dialog_id(td) => {
            // user_manager_->set_emoji_status — the premium gate is the
            // local half. `add_recent_emoji_status` (UserManager.cpp:6750-6752)
            // and `on_set_emoji_status` + the `UpdateEmojiStatusQuery`
            // send ride their steps (stub inventory): on_set_emoji_status
            // runs only from the query-success callback in the C++.
            let Some(options) = td.option_manager.as_mut() else {
                // unreachable through the request gate — the manager is
                // pinned by Td::init
                promise_unimplemented(&mut promise);
                return;
            };
            let env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            if let Err(status) = td.user_manager.set_emoji_status_local(&env) {
                promise.set_error(status);
                return;
            }
            let _ = emoji_status;
            promise_unimplemented(&mut promise);
            return;
        }
        crate::dialog_id::DialogType::Channel => {
            // chat_manager_->set_channel_emoji_status (ChatManager.cpp:3311)
            // — the flow dispatches the
            // `UpdateChannelEmojiStatusQuery` over the ChatManager net
            // handle.
            let crate::td::Td {
                global,
                option_manager,
                auth_manager,
                user_manager,
                chat_manager,
                ..
            } = td;
            let Some(options) = option_manager.as_mut() else {
                promise_unimplemented(&mut promise);
                return;
            };
            let mut env = UserEnv {
                global,
                options,
                auth_is_bot: auth_manager.is_bot(),
            };
            chat_manager.set_channel_emoji_status(
                user_manager,
                &mut env,
                dialog_id.get_channel_id(),
                emoji_status.cloned(),
                promise,
            );
            return;
        }
        _ => {}
    }
    promise.set_error(Status::error(400, "Can't change emoji status in the chat"));
}

/// `DialogManager::toggle_dialog_has_protected_content`
/// (DialogManager.cpp:2721-2761). `has_protected_content` rides the
/// net-blocked `ToggleNoForwardsQuery` send (stub inventory).
#[allow(unused_variables)]
pub fn toggle_dialog_has_protected_content(
    td: &mut Td,
    dialog_id: DialogId,
    request_message_id: Option<crate::chat_manager::MessageId>,
    has_protected_content: bool,
    mut promise: Promise<()>,
) {
    let mut access = check_dialog_access(
        td,
        dialog_id,
        false,
        AccessRights::Read,
        "toggle_dialog_has_protected_content",
    );
    if access.is_error() {
        promise.set_error(access.move_as_error());
        return;
    }

    let is_request = request_message_id.is_some();
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {}
        // get_chat_status / get_channel_status — the unloaded defaults are
        // not creators (stub inventory).
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            promise.set_error(Status::error(400, "Only owner can restrict saving content"));
            return;
        }
        crate::dialog_id::DialogType::SecretChat | crate::dialog_id::DialogType::None => {
            unreachable!("toggle_dialog_has_protected_content on a non-supergroup dialog")
        }
    }
    if is_request {
        let request_message_id = request_message_id.unwrap();
        if !request_message_id.is_server() {
            promise.set_error(Status::error(400, "Invalid message identifier specified"));
            return;
        }
    }

    // ToggleNoForwardsQuery send — [blocked: net] (stub inventory).
    promise_unimplemented(&mut promise);
}

/// `DialogManager::set_dialog_description` (DialogManager.cpp:2762-2781).
pub fn set_dialog_description(
    td: &mut Td,
    dialog_id: DialogId,
    description: &str,
    mut promise: Promise<()>,
) {
    if !have_dialog_force(td, dialog_id, "set_dialog_description") {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            promise.set_error(Status::error(400, "Can't change private chat description"));
        }
        // chat_manager_->set_chat_description / set_channel_description
        // (ChatManager.cpp:3165/:3678) — the flows dispatch the
        // `EditChatAboutQuery` over the ChatManager net handle.
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            let crate::td::Td {
                global,
                option_manager,
                auth_manager,
                user_manager,
                chat_manager,
                ..
            } = td;
            let Some(options) = option_manager.as_mut() else {
                promise_unimplemented(&mut promise);
                return;
            };
            let mut env = UserEnv {
                global,
                options,
                auth_is_bot: auth_manager.is_bot(),
            };
            match dialog_id.get_type() {
                crate::dialog_id::DialogType::Chat => chat_manager.set_chat_description(
                    user_manager,
                    &mut env,
                    dialog_id.get_chat_id(),
                    description,
                    promise,
                ),
                crate::dialog_id::DialogType::Channel => chat_manager.set_channel_description(
                    user_manager,
                    &mut env,
                    dialog_id.get_channel_id(),
                    description,
                    promise,
                ),
                _ => unreachable!("the outer match narrowed the dialog type"),
            }
        }
        crate::dialog_id::DialogType::SecretChat => {
            promise.set_error(Status::error(400, "Can't change secret chat description"));
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("set_dialog_description on DialogType::None")
        }
    }
}

// NOTE: `set_dialog_location` (DialogManager.cpp:2782-2799) is not ported
// here — its `DialogLocation` record lands with the M.5.5+ ChatManager
// integration (m54); the channel setter is a net-blocked send (stub
// inventory).

/// `DialogManager::can_delete_all_dialog_messages_by_sender`
/// (DialogManager.cpp:2876-2901).
pub fn can_delete_all_dialog_messages_by_sender(td: &mut Td, dialog_id: DialogId) -> Result<()> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User
        | crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::SecretChat
        | crate::dialog_id::DialogType::None => Result::from_status(Status::error(
            400,
            "All messages from a sender can be deleted only in supergroup chats",
        )),
        crate::dialog_id::DialogType::Channel => {
            let channel_id = dialog_id.get_channel_id();
            // is_megagroup_channel/is_monoforum_channel — unloaded `false`
            // (stub inventory), so the megagroup check fails first.
            if !is_megagroup_channel(channel_id) || is_monoforum_channel(td, dialog_id) {
                return Result::from_status(Status::error(
                    400,
                    "The method is available only in regular supergroup chats",
                ));
            }
            // get_channel_permissions — unloaded default cannot delete
            // (stub inventory).
            let _channel_status_can_delete = false;
            if !_channel_status_can_delete {
                return Result::from_status(Status::error(
                    400,
                    "Need delete messages administrator right in the supergroup chat",
                ));
            }
            Result::Ok(())
        }
    }
}

/// `DialogManager::can_pin_messages` (DialogManager.cpp:2902-2938).
pub fn can_pin_messages(td: &mut Td, dialog_id: DialogId) -> Result<()> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {}
        crate::dialog_id::DialogType::Chat => {
            // get_chat_permissions — the unloaded default cannot pin (stub
            // inventory).
            return Result::from_status(Status::error(
                400,
                "Not enough rights to manage pinned messages in the chat",
            ));
        }
        crate::dialog_id::DialogType::Channel => {
            // get_channel_permissions — the unloaded default (stub
            // inventory); the broadcast can_edit / monoforum passes are
            // unreachable past it.
            return Result::from_status(Status::error(
                400,
                "Not enough rights to manage pinned messages in the chat",
            ));
        }
        crate::dialog_id::DialogType::SecretChat => {
            return Result::from_status(Status::error(
                400,
                "Secret chats can't have pinned messages",
            ));
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("can_pin_messages on DialogType::None")
        }
    }
    if !have_input_peer(td, dialog_id, false, AccessRights::Write) {
        return Result::from_status(Status::error(400, "Not enough rights"));
    }

    Result::Ok(())
}

/// `DialogManager::can_use_premium_custom_emoji_in_dialog`
/// (DialogManager.cpp:2939-2952).
pub fn can_use_premium_custom_emoji_in_dialog(td: &mut Td, dialog_id: DialogId) -> bool {
    if td.auth_manager.is_bot() {
        return true;
    }
    if dialog_id == get_my_dialog_id(td)
        || td
            .option_manager
            .as_ref()
            .is_some_and(|options| options.get_option_boolean("is_premium", false))
    {
        return true;
    }
    // chat_manager_->can_use_premium_custom_emoji_in_channel — unloaded
    // `false` (stub inventory).
    false
}

/// `DialogManager::is_dialog_removed_from_dialog_list`
/// (DialogManager.cpp:2953-2970).
pub fn is_dialog_removed_from_dialog_list(_td: &mut Td, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {}
        // get_chat_is_active — unloaded `false` (stub inventory).
        crate::dialog_id::DialogType::Chat => return true,
        // get_channel_status — the unloaded default is not a member (stub
        // inventory).
        crate::dialog_id::DialogType::Channel => return true,
        crate::dialog_id::DialogType::SecretChat => {}
        crate::dialog_id::DialogType::None => {
            unreachable!("is_dialog_removed_from_dialog_list on DialogType::None")
        }
    }
    false
}

/// `DialogManager::on_update_dialog_bot_commands`
/// (DialogManager.cpp:2971-3004) — the terminal fan-outs land with the
/// M.5/M.8 integration (stub inventory).
pub fn on_update_dialog_bot_commands(
    td: &mut Td,
    dialog_id: DialogId,
    bot_user_id: crate::user_manager::UserId,
    _bot_commands: Vec<tdmtproto::telegram_api::BotCommand>,
) {
    if !bot_user_id.is_valid() {
        tracing::error!("Receive updateBotCommands about invalid {bot_user_id:?}");
        return;
    }
    {
        let Some(options) = td.option_manager.as_mut() else {
            return;
        };
        let mut env = UserEnv {
            global: &mut td.global,
            options,
            auth_is_bot: td.auth_manager.is_bot(),
        };
        if !td
            .user_manager
            .have_user_force(&mut env, bot_user_id, "on_update_dialog_bot_commands")
            || !td.user_manager.is_user_bot_of(bot_user_id)
        {
            return;
        }
    }
    if td.auth_manager.is_bot() {
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            if DialogId::from_user_id(bot_user_id) != dialog_id {
                tracing::error!("Receive commands of {bot_user_id:?} in {dialog_id:?}");
            }
            // user_manager_->on_update_user_commands — M.8 (stub inventory).
        }
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            // chat_manager_->on_update_chat_bot_commands /
            // on_update_channel_bot_commands — M.5.5+ (stub inventory).
        }
        crate::dialog_id::DialogType::SecretChat | crate::dialog_id::DialogType::None => {
            tracing::error!("Receive updateBotCommands in {dialog_id:?}");
        }
    }
}

/// `DialogManager::get_dialog_title` (DialogManager.cpp:2051-2065).
pub fn get_dialog_title(td: &mut Td, dialog_id: DialogId) -> String {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            td.user_manager.get_user_title_of(dialog_id.get_user_id())
        }
        // chat_manager_->get_chat_title / get_channel_title — the unloaded
        // `""` (stub inventory).
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => String::new(),
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_title_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_title on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_about` (DialogManager.cpp:2239-2255).
pub fn get_dialog_about(td: &mut Td, dialog_id: DialogId) -> String {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            let Some(options) = td.option_manager.as_mut() else {
                return String::new();
            };
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .get_user_about(&mut env, dialog_id.get_user_id())
        }
        // chat_manager_->get_chat_about / get_channel_about — the unloaded
        // `""` (stub inventory).
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => String::new(),
        crate::dialog_id::DialogType::SecretChat => {
            let Some(options) = td.option_manager.as_mut() else {
                return String::new();
            };
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .get_secret_chat_about(&mut env, dialog_id.get_secret_chat_id())
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_about on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_photo` (DialogManager.cpp:2068-2083) — the
/// C++ returns the internal `const DialogPhoto *`, which the user half maps
/// to `Option<&ProfilePhoto>` (the user_manager.rs substitution). The
/// chat/channel/secret halves need the ChatManager/secret-chat photo read
/// surface: the unloaded `None` (the C++ `nullptr`, stub inventory).
pub fn get_dialog_photo(
    td: &Td,
    dialog_id: DialogId,
) -> Option<&crate::user_manager::ProfilePhoto> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_dialog_photo(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat
        | crate::dialog_id::DialogType::Channel
        | crate::dialog_id::DialogType::SecretChat => None,
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_photo on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_accent_color_id_object`
/// (DialogManager.cpp:2085-2100) — the user/secret halves are wired. The
/// chat/channel halves are ported exactly: the C++ builds
/// `AccentColorId(chat_id)` / the unloaded `AccentColorId(channel_id)`
/// (both `id % 7`, AccentColorId.h:35-38), which is always built-in, so
/// `ThemeManager::get_accent_color_id_object` returns the raw id without
/// touching its accent-color map (ThemeManager.cpp:641-643).
pub fn get_dialog_accent_color_id_object(td: &Td, dialog_id: DialogId) -> i32 {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_accent_color_id_object_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat => (dialog_id.get_chat_id().get() % 7) as i32,
        crate::dialog_id::DialogType::Channel => (dialog_id.get_channel_id().get() % 7) as i32,
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_accent_color_id_object_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_accent_color_id_object on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_background_custom_emoji_id`
/// (DialogManager.cpp:2102-2117) — the user/secret halves are wired; the
/// chat/channel halves are the C++ constants
/// (`ChatManager.cpp:2970-2972, 2974-2978` return `CustomEmojiId()`
/// without a chat read).
pub fn get_dialog_background_custom_emoji_id(
    td: &Td,
    dialog_id: DialogId,
) -> crate::emoji_status::CustomEmojiId {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_background_custom_emoji_id_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            crate::emoji_status::CustomEmojiId::default()
        }
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_background_custom_emoji_id_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_background_custom_emoji_id on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_upgraded_gift_colors_object`
/// (DialogManager.cpp:2119-2136) — the user/secret halves are wired; the
/// chat/channel halves are the C++ `nullptr` (no gift colors on groups).
pub fn get_dialog_upgraded_gift_colors_object(
    td: &Td,
    dialog_id: DialogId,
) -> Option<crate::td_api::UpgradedGiftColors> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_upgraded_gift_colors_object_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => None,
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_upgraded_gift_colors_object_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_upgraded_gift_colors_object on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_profile_accent_color_id_object`
/// (DialogManager.cpp:2137-2153) — the user/secret halves are wired; the
/// chat/channel halves are the C++ `-1` constants
/// (ChatManager.cpp:2986-2994).
pub fn get_dialog_profile_accent_color_id_object(td: &Td, dialog_id: DialogId) -> i32 {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_profile_accent_color_id_object_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => -1,
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_profile_accent_color_id_object_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_profile_accent_color_id_object on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_profile_background_custom_emoji_id`
/// (DialogManager.cpp:2154-2170) — the user/secret halves are wired; the
/// chat/channel halves are the C++ unloaded `CustomEmojiId()`
/// (ChatManager.cpp:3001-3015).
pub fn get_dialog_profile_background_custom_emoji_id(
    td: &Td,
    dialog_id: DialogId,
) -> crate::emoji_status::CustomEmojiId {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_profile_background_custom_emoji_id_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            crate::emoji_status::CustomEmojiId::default()
        }
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_profile_background_custom_emoji_id_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_profile_background_custom_emoji_id on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_status` (DialogManager.cpp:2171-2187) — the
/// user/secret arms are the C++ `DialogParticipantStatus::Member(0, "")`;
/// the chat/channel arms are the unloaded `Banned(0, "")`
/// (ChatManager.cpp:8559-8562 / 8680-8686, stub inventory).
pub fn get_dialog_status(
    _td: &Td,
    dialog_id: DialogId,
) -> crate::chat_manager::DialogParticipantStatus {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User | crate::dialog_id::DialogType::SecretChat => {
            crate::chat_manager::DialogParticipantStatus::member(0, String::new())
        }
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            crate::chat_manager::DialogParticipantStatus::banned(0, String::new())
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_status on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_permissions` (DialogManager.cpp:2188-2204) —
/// same shape as [`get_dialog_status`].
pub fn get_dialog_permissions(
    _td: &Td,
    dialog_id: DialogId,
) -> crate::chat_manager::DialogParticipantStatus {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User | crate::dialog_id::DialogType::SecretChat => {
            crate::chat_manager::DialogParticipantStatus::member(0, String::new())
        }
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            crate::chat_manager::DialogParticipantStatus::banned(0, String::new())
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_permissions on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_default_permissions`
/// (DialogManager.cpp:2205-2221) — the unloaded C++ result is
/// `RestrictedRights::restrict_all()` on every arm (UserManager.cpp:5610-5615,
/// 5620-5624; ChatManager.cpp:3041-3055); the loaded user/secret halves
/// land with the user-default-permissions helper (stub inventory).
pub fn get_dialog_default_permissions(
    _td: &Td,
    dialog_id: DialogId,
) -> crate::chat_manager::RestrictedRights {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => crate::chat_manager::RestrictedRights::restrict_all(),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
            crate::chat_manager::RestrictedRights::restrict_all()
        }
        crate::dialog_id::DialogType::SecretChat => {
            crate::chat_manager::RestrictedRights::restrict_all()
        }
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_default_permissions on DialogType::None")
        }
    }
}

/// `DialogManager::get_dialog_emoji_status_object`
/// (DialogManager.cpp:2222-2237) — the user/secret halves are wired; the
/// chat/channel halves are the C++ `nullptr` (ChatManager.cpp:3057-3066).
pub fn get_dialog_emoji_status_object(
    td: &Td,
    dialog_id: DialogId,
) -> Option<crate::td_api::EmojiStatus> {
    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => td
            .user_manager
            .get_user_emoji_status_object_of(dialog_id.get_user_id()),
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => None,
        crate::dialog_id::DialogType::SecretChat => td
            .user_manager
            .get_secret_chat_emoji_status_object_of(dialog_id.get_secret_chat_id()),
        crate::dialog_id::DialogType::None => {
            unreachable!("get_dialog_emoji_status_object on DialogType::None")
        }
    }
}

/// `DialogManager::CheckDialogUsernameResult` (DialogManager.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckDialogUsernameResult {
    Ok,
    Invalid,
    Occupied,
    Purchasable,
    PublicDialogsTooMany,
    PublicGroupsUnavailable,
}

/// `DialogManager::on_dialog_usernames_updated`
/// (DialogManager.cpp:3005-3017) — a [`DialogManager`] method (the manager
/// state is detached for the call). Called from the UserManager/ChatManager
/// username intakes; their fan-ins arrive with those steps.
#[allow(dead_code)]
impl DialogManager {
    pub(crate) fn on_dialog_usernames_updated(
        &mut self,
        dialog_id: DialogId,
        old_usernames: &[String],
        new_usernames: &[String],
    ) {
        for username in old_usernames {
            let cleaned_username = clean_username(username);
            self.resolved_usernames.remove(&cleaned_username);
            self.inaccessible_resolved_usernames
                .remove(&cleaned_username);
        }

        self.on_dialog_usernames_received(dialog_id, new_usernames, false);
    }

    /// `DialogManager::on_dialog_usernames_received`
    /// (DialogManager.cpp:3018-3027).
    pub(crate) fn on_dialog_usernames_received(
        &mut self,
        dialog_id: DialogId,
        usernames: &[String],
        from_database: bool,
    ) {
        for username in usernames {
            let cleaned_username = clean_username(username);
            if !cleaned_username.is_empty() {
                self.resolved_usernames.insert(
                    cleaned_username,
                    ResolvedUsername {
                        dialog_id,
                        expires_at: time::now()
                            + if from_database {
                                0.0
                            } else {
                                USERNAME_CACHE_EXPIRE_TIME as f64
                            },
                    },
                );
            }
        }
    }
}

/// `DialogManager::check_dialog_username` (DialogManager.cpp:3028-3112) —
/// the local gates and the empty/invalid answers are faithful; the
/// `CheckUsernameQuery`/`CheckChannelUsernameQuery` sends are [blocked: net]
/// (stub inventory) and their error→result mapping is net-step residue.
pub fn check_dialog_username(
    td: &mut Td,
    dialog_id: DialogId,
    username: &str,
    is_bot: bool,
    mut promise: Promise<crate::td_api::Object>,
) {
    if dialog_id != DialogId::new(0)
        && dialog_id.get_type() != crate::dialog_id::DialogType::User
        && !have_dialog_force(td, dialog_id, "check_dialog_username")
    {
        promise.set_error(Status::error(400, "Chat not found"));
        return;
    }

    match dialog_id.get_type() {
        crate::dialog_id::DialogType::User => {
            if dialog_id != get_my_dialog_id(td) {
                promise.set_error(Status::error(
                    400,
                    "Can't check username for private chat with other user",
                ));
                return;
            }
        }
        crate::dialog_id::DialogType::Channel => {
            // get_channel_status — the unloaded default is not a creator
            // (stub inventory); the editable-username fast path is
            // unreachable past it.
            promise.set_error(Status::error(400, "Not enough rights to change username"));
            return;
        }
        crate::dialog_id::DialogType::None => {}
        crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::SecretChat => {
            if !username.is_empty() {
                promise.set_error(Status::error(400, "The chat can't have a username"));
                return;
            }
        }
    }

    if username.is_empty() {
        let result = if is_bot {
            CheckDialogUsernameResult::Invalid
        } else {
            CheckDialogUsernameResult::Ok
        };
        promise.set_value(get_check_chat_username_result_object(result));
        return;
    }

    if !crate::misc::is_allowed_username(username) && username.len() != 4 {
        promise.set_value(get_check_chat_username_result_object(
            CheckDialogUsernameResult::Invalid,
        ));
        return;
    }

    // CheckUsernameQuery / CheckChannelUsernameQuery send — [blocked: net]
    // (stub inventory).
    promise_unimplemented(&mut promise);
}

/// `DialogManager::get_check_chat_username_result_object`
/// (DialogManager.cpp:3113-3133).
pub fn get_check_chat_username_result_object(
    result: CheckDialogUsernameResult,
) -> crate::td_api::Object {
    use crate::td_api;
    match result {
        CheckDialogUsernameResult::Ok => {
            td_api::Object::checkChatUsernameResultOk(td_api::checkChatUsernameResultOk {})
        }
        CheckDialogUsernameResult::Invalid => {
            td_api::Object::checkChatUsernameResultUsernameInvalid(
                td_api::checkChatUsernameResultUsernameInvalid {},
            )
        }
        CheckDialogUsernameResult::Occupied => {
            td_api::Object::checkChatUsernameResultUsernameOccupied(
                td_api::checkChatUsernameResultUsernameOccupied {},
            )
        }
        CheckDialogUsernameResult::Purchasable => {
            td_api::Object::checkChatUsernameResultUsernamePurchasable(
                td_api::checkChatUsernameResultUsernamePurchasable {},
            )
        }
        CheckDialogUsernameResult::PublicDialogsTooMany => {
            td_api::Object::checkChatUsernameResultPublicChatsTooMany(
                td_api::checkChatUsernameResultPublicChatsTooMany {},
            )
        }
        CheckDialogUsernameResult::PublicGroupsUnavailable => {
            td_api::Object::checkChatUsernameResultPublicGroupsUnavailable(
                td_api::checkChatUsernameResultPublicGroupsUnavailable {},
            )
        }
    }
}

/// `DialogManager::get_resolved_dialog_by_username`
/// (DialogManager.cpp:3232-3240) — a [`DialogManager`] method; the callers
/// (resolve_dialog, the RecentDialogList `@username` load) arrive with the
/// net step.
#[allow(dead_code)]
impl DialogManager {
    pub(crate) fn get_resolved_dialog_by_username(&self, username: &str) -> DialogId {
        let cleaned_username = clean_username(username);
        match self.resolved_usernames.get(&cleaned_username) {
            Some(resolved_username) if resolved_username.dialog_id.is_valid() => {
                return resolved_username.dialog_id;
            }
            _ => {}
        }

        self.inaccessible_resolved_usernames
            .get(&cleaned_username)
            .copied()
            .unwrap_or_else(|| DialogId::new(0))
    }

    /// `DialogManager::resolve_dialog_username`
    /// (DialogManager.cpp:3242-3256) — the query path is net-blocked, so the
    /// passed promise completes with the net-blocked error instead of
    /// parking in the C++ `resolve_dialog_username_queries_` queue (stub
    /// inventory). `resolve_dialog` itself (DialogManager.cpp:3185-3207,
    /// the username/channel-id resolution entry) lands with the net step.
    pub(crate) fn resolve_dialog_username(
        &mut self,
        _td: &mut Td,
        username: &str,
        promise: &mut Promise<()>,
    ) -> DialogId {
        match self.resolved_usernames.get(username) {
            Some(resolved_username) if resolved_username.dialog_id.is_valid() => {
                let dialog_id = resolved_username.dialog_id;
                if resolved_username.expires_at < time::now() {
                    // the refresh query with an empty promise — [blocked:
                    // net]
                    tracing::info!("Refresh resolved username — [blocked: net]");
                }
                return dialog_id;
            }
            _ => {}
        }
        let dialog_id = self
            .inaccessible_resolved_usernames
            .get(username)
            .copied()
            .unwrap_or_else(|| DialogId::new(0));
        if !dialog_id.is_valid() {
            promise_unimplemented(promise);
        }
        dialog_id
    }
}

/// `DialogManager::drop_username` (DialogManager.cpp:3307-3324) — free fn
/// over the detached manager.
pub fn drop_username(td: &mut Td, username: &str) {
    let cleaned_username = clean_username(username);
    if cleaned_username.is_empty() {
        return;
    }

    let mut manager = std::mem::take(&mut td.dialog_manager);
    manager
        .inaccessible_resolved_usernames
        .remove(&cleaned_username);

    match manager.resolved_usernames.get(&cleaned_username) {
        Some(resolved_username) if resolved_username.dialog_id.is_valid() => {
            // C++: when the resolved dialog's input peer is readable it
            // reloads the full info here (reload_dialog_info_full,
            // DialogManager.cpp:1656-1682) — that reload lands with the
            // user/chat/channel full-reload families (§3.3/§3.4).
            manager.resolved_usernames.remove(&cleaned_username);
        }
        _ => {}
    }
    td.dialog_manager = manager;
}

/// `DialogManager::reload_video_chat_on_search`
/// (DialogManager.cpp:3296-3305).
pub fn reload_video_chat_on_search(td: &mut Td, username: &str) {
    if !td.auth_manager.is_authorized() {
        return;
    }

    let cleaned_username = clean_username(username);
    if !cleaned_username.is_empty() {
        let mut manager = std::mem::take(&mut td.dialog_manager);
        manager
            .reload_video_chat_on_search_usernames
            .insert(cleaned_username);
        td.dialog_manager = manager;
    }
}

/// `DialogManager::search_public_dialog` (DialogManager.cpp:3258-3294).
pub fn search_public_dialog(
    td: &mut Td,
    username_to_search: &str,
    force: bool,
    mut promise: Promise<()>,
) -> DialogId {
    let mut username = clean_username(username_to_search);
    if let Some(stripped) = username.strip_prefix('@') {
        username = stripped.to_string();
    }
    if username.is_empty() {
        promise.set_error(Status::error(200, "Username is invalid"));
        return DialogId::new(0);
    }

    let dialog_id = {
        let mut manager = std::mem::take(&mut td.dialog_manager);
        let dialog_id = manager.resolve_dialog_username(td, &username, &mut promise);
        td.dialog_manager = manager;
        dialog_id
    };
    if !dialog_id.is_valid() {
        return DialogId::new(0);
    }

    if have_input_peer(td, dialog_id, false, AccessRights::Read) {
        let mut manager = std::mem::take(&mut td.dialog_manager);
        if !force
            && manager
                .reload_video_chat_on_search_usernames
                .remove(&username)
            && dialog_id.get_type() == crate::dialog_id::DialogType::Channel
        {
            // chat_manager_->reload_channel_full — the promise completes
            // with the net-blocked error (stub inventory).
            td.dialog_manager = manager;
            promise.set_error(Status::error(500, "Network is not implemented yet"));
            return DialogId::new(0);
        }
        td.dialog_manager = manager;

        // messages_manager_->create_dialog — the local create completes for
        // a known dialog (force_create_dialog is the M.6 no-op; stub
        // inventory).
        promise.set_value(());
        return dialog_id;
    }

    if force || dialog_id.get_type() != crate::dialog_id::DialogType::User {
        // bot username may be known despite there is no access_hash
        force_create_dialog(td, dialog_id, "search_public_dialog");
        promise.set_value(());
        return dialog_id;
    }

    // send_resolve_dialog_username_query — [blocked: net] (stub inventory).
    promise.set_error(Status::error(500, "Network is not implemented yet"));
    DialogId::new(0)
}

/// `DialogManager::search_public_dialogs` (DialogManager.cpp:3356-3404).
pub fn search_public_dialogs(
    td: &mut Td,
    query: &str,
    chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
    mut promise: Promise<()>,
) -> Vec<DialogId> {
    let type_filter = get_dialog_type_filter(chat_type_filter);

    let query_length = query.chars().count();
    if query_length < MIN_SEARCH_PUBLIC_DIALOG_PREFIX_LEN
        || (query_length == MIN_SEARCH_PUBLIC_DIALOG_PREFIX_LEN
            && query.as_bytes().first() == Some(&b'@'))
    {
        let mut username = clean_username(query);
        if let Some(stripped) = username.strip_prefix('@') {
            username = stripped.to_string();
        }

        for &short_username in get_valid_short_usernames() {
            if 2 * username.len() > short_username.len() && short_username.starts_with(&username) {
                let username = short_username;
                let dialog_id = {
                    let mut manager = std::mem::take(&mut td.dialog_manager);
                    let dialog_id = manager.resolve_dialog_username(td, username, &mut promise);
                    td.dialog_manager = manager;
                    dialog_id
                };
                if !dialog_id.is_valid() {
                    return Vec::new();
                }

                force_create_dialog(td, dialog_id, "search_public_dialogs");

                // messages_manager_->can_add_dialog_to_filter — Ok (stub
                // inventory). The C++ short-circuits on the type first
                // (cpp:3382-3383), so `get_user_id` — an assert of
                // `type == User` — is only evaluated for user dialogs; the
                // single-arg `is_user_contact` is `is_contact` only.
                if dialog_id.get_type() == crate::dialog_id::DialogType::User
                    && td
                        .user_manager
                        .is_user_contact_of(dialog_id.get_user_id(), false)
                {
                    continue;
                }
                if !is_dialog_suitable_for_type_filter(td, dialog_id, type_filter) {
                    continue;
                }

                promise.set_value(());
                return vec![dialog_id];
            }
        }
        promise.set_value(());
        return Vec::new();
    }

    let type_num = type_filter as usize;
    let manager = std::mem::take(&mut td.dialog_manager);
    if let Some(found) = manager.found_public_dialogs[type_num].get(query) {
        let found = found.clone();
        td.dialog_manager = manager;
        promise.set_value(());
        return found;
    }
    td.dialog_manager = manager;

    // send_search_public_dialogs_query — [blocked: net] (stub inventory);
    // the C++ promise queue exists only while the query is in flight.
    promise.set_error(Status::error(500, "Network is not implemented yet"));
    Vec::new()
}

/// `DialogManager::search_dialogs_on_server` (DialogManager.cpp:3406-3433).
pub fn search_dialogs_on_server(
    td: &mut Td,
    query: &str,
    chat_type_filter: Option<&crate::td_api::SearchChatTypeFilter>,
    mut limit: i32,
    mut promise: Promise<()>,
) -> Vec<DialogId> {
    let type_filter = get_dialog_type_filter(chat_type_filter);

    if limit < 0 {
        promise.set_error(Status::error(400, "Limit must be non-negative"));
        return Vec::new();
    }
    if limit > MAX_GET_DIALOGS {
        limit = MAX_GET_DIALOGS;
    }

    if query.is_empty() {
        promise.set_value(());
        return Vec::new();
    }

    let type_num = type_filter as usize;
    let manager = std::mem::take(&mut td.dialog_manager);
    if let Some(found) = manager.found_on_server_dialogs[type_num].get(query) {
        // messages_manager_->sort_dialogs_by_order — no order source in the
        // port, the truncation is the observable part (stub inventory).
        let found = found[..found.len().min(limit.max(0) as usize)].to_vec();
        td.dialog_manager = manager;
        promise.set_value(());
        return found;
    }
    td.dialog_manager = manager;

    // send_search_public_dialogs_query — [blocked: net] (stub inventory).
    promise.set_error(Status::error(500, "Network is not implemented yet"));
    Vec::new()
}

/// `DialogManager::on_get_public_dialogs_search_result` /
/// `on_failed_public_dialogs_search` (DialogManager.cpp:3449-3479) — the
/// cache fills land with the net step; the negative-cache shape is recorded
/// here for the integration.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog_id::DialogType;
    use crate::td::TdCallback;
    use crate::td_api;
    use tdutils::promise::PromiseCreator;

    /// A drop-everything callback (the td.rs tests' CollectCallback, minus
    /// the collection).
    #[derive(Default)]
    struct DropCallback;

    impl TdCallback for DropCallback {
        fn on_result(&mut self, _id: u64, _result: crate::td_api::Object) {}
        fn on_error(&mut self, _id: u64, _error: crate::td_api::Error) {}
    }

    fn test_td() -> crate::td::Td {
        crate::td::Td::new(Box::new(DropCallback))
    }

    fn unit_promise() -> Promise<()> {
        PromiseCreator::lambda(|()| ())
    }

    #[test]
    fn input_peer_none_dialog_is_input_peer_empty() {
        let mut td = test_td();
        let input_peer = get_input_peer(&mut td, DialogId::new(0), AccessRights::Read);
        assert!(matches!(
            input_peer,
            Some(tdmtproto::telegram_api::InputPeer::inputPeerEmpty(_))
        ));
    }

    #[test]
    fn get_input_peer_force_builds_the_zero_access_hash_form() {
        use tdmtproto::telegram_api;
        let peer =
            get_input_peer_force(DialogId::from_user_id(crate::user_manager::UserId::new(7)));
        assert!(matches!(peer, telegram_api::InputPeer::inputPeerUser(_)));
        if let telegram_api::InputPeer::inputPeerUser(inner) = peer {
            assert_eq!(inner.user_id, 7);
            assert_eq!(inner.access_hash, 0);
        }

        // the dialog id of a basic group is the negated chat id
        let peer =
            get_input_peer_force(DialogId::from_chat_id(crate::chat_manager::ChatId::new(5)));
        if let telegram_api::InputPeer::inputPeerChat(inner) = peer {
            assert_eq!(inner.chat_id, 5);
        } else {
            panic!("wrong peer type");
        }
    }

    #[test]
    fn get_input_dialog_peer_wraps_the_input_peer() {
        let mut td = test_td();
        let dialog_peer = get_input_dialog_peer(&mut td, DialogId::new(0), AccessRights::Read);
        assert!(matches!(
            dialog_peer,
            Some(tdmtproto::telegram_api::InputDialogPeer::inputDialogPeer(_))
        ));
        // secret chats never produce an input dialog peer
        assert!(
            get_input_dialog_peer(
                &mut td,
                DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1)),
                AccessRights::Read
            )
            .is_none()
        );
    }

    #[test]
    fn check_dialog_access_reports_missing_chats() {
        let mut td = test_td();
        let status =
            check_dialog_access(&mut td, DialogId::new(0), true, AccessRights::Read, "test")
                .error()
                .clone();
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Invalid chat identifier specified");

        let status = check_dialog_access(
            &mut td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(100)),
            true,
            AccessRights::Read,
            "test",
        )
        .error()
        .clone();
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Chat not found");
    }

    #[test]
    fn check_dialog_access_in_memory_reports_write_access() {
        let mut td = test_td();
        // an inaccessible user chat: the in-memory check runs past the
        // have_dialog_force stub only for accessible users, so exercise the
        // secret-chat and write branches directly
        let secret_dialog =
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1));
        let status =
            check_dialog_access_in_memory(&mut td, secret_dialog, false, AccessRights::Read)
                .error()
                .clone();
        assert_eq!(status.message(), "Not supported in secret chats");

        // the secret-chat message wins over the write-access one (the C++
        // checks the type first), so the Edit arm needs a non-secret dialog
        let channel_dialog = DialogId::from_channel_id(crate::chat_manager::ChannelId::new(1001));
        let status =
            check_dialog_access_in_memory(&mut td, channel_dialog, false, AccessRights::Edit)
                .error()
                .clone();
        assert_eq!(status.message(), "Have no write access to the chat");

        let status =
            check_dialog_access_in_memory(&mut td, channel_dialog, false, AccessRights::Read)
                .error()
                .clone();
        assert_eq!(status.message(), "Can't access the chat");
        let _ = DialogType::None;
    }

    #[test]
    fn get_chat_type_object_shapes() {
        let mut td = test_td();
        // init the option manager so the id-object builders can run
        init_td_db_for_test(&mut td, "m6_chat_type_object");

        let chat_type = get_chat_type_object(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(42)),
            "test",
        );
        assert!(matches!(chat_type, td_api::ChatType::chatTypePrivate(_)));

        let chat_type = get_chat_type_object(
            &mut td,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(1001)),
            "test",
        );
        match chat_type {
            td_api::ChatType::chatTypeSupergroup(inner) => {
                assert_eq!(inner.supergroup_id, 1001);
                assert!(inner.is_channel, "unloaded channels are not megagroups");
            }
            _ => panic!("wrong chat type"),
        }

        // the basic-group arm is the id-object form
        let chat_type = get_chat_type_object(
            &mut td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(5)),
            "test",
        );
        assert!(matches!(chat_type, td_api::ChatType::chatTypeBasicGroup(_)));

        // the secret-chat arm answers the secret shape (the unloaded secret
        // chat has no peer user, so the user half is the invalid id)
        let chat_type = get_chat_type_object(
            &mut td,
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1)),
            "test",
        );
        match chat_type {
            td_api::ChatType::chatTypeSecret(inner) => {
                assert_eq!(inner.secret_chat_id, 1);
                assert_eq!(inner.user_id, 0);
            }
            _ => panic!("wrong chat type"),
        }
    }

    #[test]
    fn get_chats_object_shapes() {
        let dialog_ids = vec![
            DialogId::from_user_id(crate::user_manager::UserId::new(1)),
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(2)),
        ];
        let chats = get_chats_object(-1, &dialog_ids, "test");
        assert_eq!(chats.total_count, 2);
        assert_eq!(chats.chat_ids, vec![1, -2]);

        let chats = get_chats_object(99, &dialog_ids, "test");
        assert_eq!(chats.total_count, 99);
    }

    #[test]
    fn predicates_use_unloaded_channel_results() {
        let mut td = test_td();
        let channel_dialog = DialogId::from_channel_id(crate::chat_manager::ChannelId::new(1001));
        assert!(!is_broadcast_channel(&mut td, channel_dialog));
        assert!(!is_megagroup_channel(channel_dialog.get_channel_id()));
        assert!(!is_group_dialog(&mut td, channel_dialog));
        assert!(
            can_dialog_have_threads(&mut td, channel_dialog),
            "not broadcast → threads possible"
        );
        assert!(!is_forum_channel(&mut td, channel_dialog));
        assert_eq!(
            get_dialog_notification_setting_scope(&mut td, channel_dialog),
            NotificationSettingsScope::Group
        );
        assert!(!is_anonymous_administrator(&mut td, channel_dialog));
    }

    #[test]
    fn predicates_answer_every_dialog_type() {
        let mut td = test_td();
        // basic groups are groups but never have threads
        let chat_dialog = DialogId::from_chat_id(crate::chat_manager::ChatId::new(5));
        assert!(is_group_dialog(&mut td, chat_dialog));
        assert!(!can_dialog_have_threads(&mut td, chat_dialog));
        assert_eq!(
            get_dialog_notification_setting_scope(&mut td, chat_dialog),
            NotificationSettingsScope::Group
        );
        assert!(!is_anonymous_administrator(&mut td, chat_dialog));

        // a user dialog is a group only in a bot client; threads follow the
        // bot conjunction; the validity CHECK of the non-channel arm passes
        let user_dialog = DialogId::from_user_id(crate::user_manager::UserId::new(7));
        assert!(!is_group_dialog(&mut td, user_dialog));
        assert!(!can_dialog_have_threads(&mut td, user_dialog));
        assert_eq!(
            get_dialog_notification_setting_scope(&mut td, user_dialog),
            NotificationSettingsScope::Private
        );
        assert!(!is_anonymous_administrator(&mut td, user_dialog));

        // secret chats answer the private scope and no threads; the invalid
        // dialog is rejected by the validity CHECK
        let secret_dialog =
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1));
        assert!(!is_group_dialog(&mut td, secret_dialog));
        assert!(!can_dialog_have_threads(&mut td, secret_dialog));
        assert_eq!(
            get_dialog_notification_setting_scope(&mut td, secret_dialog),
            NotificationSettingsScope::Private
        );
        assert!(!is_anonymous_administrator(&mut td, secret_dialog));
    }

    #[test]
    #[should_panic(expected = "CHECK failed: dialog_id.is_valid()")]
    fn is_anonymous_administrator_rejects_invalid_dialogs() {
        let mut td = test_td();
        let _ = is_anonymous_administrator(&mut td, DialogId::new(0));
    }

    #[test]
    fn dialog_getters_answer_the_unloaded_results() {
        let td = test_td();
        // user/secret arms are members; unloaded chat/channel arms are the
        // C++ Banned(0, "")
        let status = get_dialog_status(
            &td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
        );
        assert!(status.is_member());
        assert!(!status.is_banned());
        let status = get_dialog_status(
            &td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(5)),
        );
        assert!(!status.is_member());
        assert!(status.is_banned());
        let permissions = get_dialog_permissions(
            &td,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(5)),
        );
        assert!(permissions.is_banned());

        // default permissions are restrict_all on every unloaded arm
        let default_permissions = get_dialog_default_permissions(
            &td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
        );
        assert!(!default_permissions.can_send_messages());

        // the accent id of a chat/channel is the raw id modulo 7
        // (AccentColorId(ChatId), AccentColorId.h:35-38 — always built-in)
        assert_eq!(
            get_dialog_accent_color_id_object(
                &td,
                DialogId::from_chat_id(crate::chat_manager::ChatId::new(12))
            ),
            12 % 7
        );
        assert_eq!(
            get_dialog_accent_color_id_object(
                &td,
                DialogId::from_channel_id(crate::chat_manager::ChannelId::new(100))
            ),
            100 % 7
        );
        // the group profile-accent constant is -1
        assert_eq!(
            get_dialog_profile_accent_color_id_object(
                &td,
                DialogId::from_chat_id(crate::chat_manager::ChatId::new(5))
            ),
            -1
        );

        // groups have no gift colors / emoji status / background emoji
        let chat_dialog = DialogId::from_chat_id(crate::chat_manager::ChatId::new(5));
        assert!(get_dialog_upgraded_gift_colors_object(&td, chat_dialog).is_none());
        assert!(get_dialog_emoji_status_object(&td, chat_dialog).is_none());
        assert_eq!(
            get_dialog_background_custom_emoji_id(&td, chat_dialog),
            crate::emoji_status::CustomEmojiId::default()
        );
        // unloaded chat/channel photos are the C++ nullptr
        assert!(get_dialog_photo(&td, chat_dialog).is_none());
    }

    #[test]
    fn on_get_dialog_error_classifies_messages() {
        let mut td = test_td();
        let dialog_id = DialogId::from_user_id(crate::user_manager::UserId::new(1));

        assert!(on_get_dialog_error(
            &mut td,
            dialog_id,
            &Status::error(400, "BOT_METHOD_INVALID"),
            "test"
        ));
        assert!(on_get_dialog_error(
            &mut td,
            dialog_id,
            &Status::error(401, "UNAUTHORIZED"),
            "test"
        ));
        assert!(on_get_dialog_error(
            &mut td,
            dialog_id,
            &Status::error(400, "ALLOW_PAYMENT_REQUIRED_X"),
            "test"
        ));
        assert!(!on_get_dialog_error(
            &mut td,
            dialog_id,
            &Status::error(400, "SOMETHING_ELSE"),
            "test"
        ));

        // the channel arm ports the status-message heads of
        // ChatManager::on_get_channel_error: CHANNEL_MONOFORUM_UNSUPPORTED
        // answers true even with every channel unloaded
        // (ChatManager.cpp:6398-6400), everything else is the unloaded false
        let channel_dialog = DialogId::from_channel_id(crate::chat_manager::ChannelId::new(1001));
        assert!(on_get_dialog_error(
            &mut td,
            channel_dialog,
            &Status::error(400, "CHANNEL_MONOFORUM_UNSUPPORTED"),
            "test"
        ));
        assert!(!on_get_dialog_error(
            &mut td,
            channel_dialog,
            &Status::error(400, "SOMETHING_ELSE"),
            "test"
        ));
        assert!(on_get_dialog_error(
            &mut td,
            channel_dialog,
            &Status::error(400, "BOT_METHOD_INVALID"),
            "test"
        ));
    }

    #[test]
    fn recently_found_dialogs_round_trip_through_the_pmc() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_recent_dialogs");

        let user_id = crate::user_manager::UserId::new(2200000042);
        let options = td.option_manager.as_mut().expect("user environment");
        let mut env = UserEnv {
            global: &mut td.global,
            options,
            auth_is_bot: td.auth_manager.is_bot(),
        };
        let seeded = td.user_manager.on_get_user(
            &mut env,
            tdmtproto::telegram_api::User::user(tdmtproto::telegram_api::user {
                id: user_id.get(),
                flags: 1, // USER_ACCESS_HASH_MASK — a flagless user is a min user
                access_hash: 123,
                first_name: "Rec".to_string(),
                ..Default::default()
            }),
            "test",
        );
        assert_eq!(seeded, user_id);

        let dialog_id = DialogId::from_user_id(user_id);
        add_recently_found_dialog(&mut td, dialog_id).unwrap();
        // second add of the same dialog moves it to the top once
        add_recently_found_dialog(&mut td, dialog_id).unwrap();

        // the pmc carries the id
        assert_eq!(
            td.global
                .binlog_pmc()
                .get("recently_found_dialog_usernames_and_ids"),
            "2200000042"
        );

        let (total, ids) = search_recently_found_dialogs(&mut td, "", None, 10, unit_promise());
        assert_eq!(total, 1);
        assert_eq!(ids, vec![dialog_id]);

        // a fresh list over the same database reloads the id (the user is
        // still in memory, so the load-time filter keeps it)
        let mut reloaded = RecentDialogList::new("recently_found", MAX_RECENT_DIALOGS);
        let (total, ids) = reloaded.get_dialogs(&mut td, 10);
        assert_eq!(total, 1);
        assert_eq!(ids, vec![dialog_id]);

        // removal persists too
        remove_recently_found_dialog(&mut td, dialog_id).unwrap();
        assert_eq!(
            td.global
                .binlog_pmc()
                .get("recently_found_dialog_usernames_and_ids"),
            ""
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_dialogs_uses_hints_and_negative_keys() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_search_dialogs");

        let user_id = crate::user_manager::UserId::new(2200000043);
        let options = td.option_manager.as_mut().expect("user environment");
        let mut env = UserEnv {
            global: &mut td.global,
            options,
            auth_is_bot: td.auth_manager.is_bot(),
        };
        td.user_manager.on_get_user(
            &mut env,
            tdmtproto::telegram_api::User::user(tdmtproto::telegram_api::user {
                id: user_id.get(),
                flags: 1, // USER_ACCESS_HASH_MASK — a flagless user is a min user
                access_hash: 123,
                first_name: "Alice".to_string(),
                last_name: "Searchable".to_string(),
                ..Default::default()
            }),
            "test",
        );

        let dialog_id = DialogId::from_user_id(user_id);
        add_dialog_to_hints(&mut td, dialog_id);

        let (total, ids) = search_dialogs(&mut td, "alice", None, 5, unit_promise());
        assert_eq!(total, 1);
        assert_eq!(ids, vec![dialog_id]);

        // empty query goes to the (empty) recently-found list
        let (total, ids) = search_dialogs(&mut td, "", None, 5, unit_promise());
        assert_eq!((total, ids), (0, Vec::new()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Open a throwaway database and initialize the Td global + option
    /// manager the way `Td::init` does (td.rs:340-390).
    fn init_td_db_for_test(td: &mut crate::td::Td, name: &str) -> String {
        use tddb::td_db::Parameters as DbParameters;
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m6-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let opened = tddb::td_db::TdDb::open(DbParameters {
            database_directory: dir.clone(),
            ..Default::default()
        })
        .unwrap();
        let mut database = opened.database;
        let is_test_dc = database.parameters().is_test_dc;
        let config_pmc = database.take_config_pmc();
        td.global.init(database);
        let mut option_manager = crate::option_manager::OptionManager::new(is_test_dc);
        option_manager.attach_pmc(
            config_pmc,
            td.global.unix_time(),
            td.global.get_server_time_difference(),
        );
        td.option_manager = Some(option_manager);
        dir
    }

    // --- M.6.18/M.6.19 ---

    use std::sync::{Arc, Mutex};

    /// The slot half of the request-promise collapse, aliased to match
    /// requests.rs `ObjectSlot`.
    type UnitSlot = Arc<Mutex<Option<Result<()>>>>;
    type ObjectSlot = Arc<Mutex<Option<Result<crate::td_api::Object>>>>;

    /// The requests.rs unit-request-promise collapse, for tests.
    fn capture_unit_promise() -> (Promise<()>, UnitSlot) {
        let slot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = PromiseCreator::lambda_result(move |result: Result<()>| {
            *capture.lock().unwrap() = Some(result);
        });
        (promise, slot)
    }

    fn captured_status<T>(slot: &Arc<Mutex<Option<Result<T>>>>) -> Status {
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => status,
            Some(Result::Ok(_)) => panic!("expected an error, got Ok"),
            None => panic!("the promise was never completed"),
        }
    }

    /// The object-shaped counterpart of `capture_unit_promise`, for the
    /// td_api handler entry points that answer with `Promise<Object>`.
    fn capture_object_promise() -> (Promise<crate::td_api::Object>, ObjectSlot) {
        let slot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise =
            PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                *capture.lock().unwrap() = Some(result);
            });
        (promise, slot)
    }

    #[test]
    fn on_dialog_opened_and_deleted_drive_the_recent_lists() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_recent_opened");

        // the opened list persists through the binlog pmc, and its
        // update-time liveness filter keeps only dialogs with known info —
        // seed the user so the added dialog survives the filter.
        let user_id = crate::user_manager::UserId::new(42);
        {
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager.on_get_user(
                &mut env,
                tdmtproto::telegram_api::User::user(tdmtproto::telegram_api::user {
                    id: user_id.get(),
                    flags: 1, // USER_ACCESS_HASH_MASK — a flagless user is a min user
                    access_hash: 123,
                    first_name: "Recent".to_string(),
                    ..Default::default()
                }),
                "test",
            );
        }

        let dialog_id = DialogId::from_user_id(user_id);
        {
            let mut manager = std::mem::take(&mut td.dialog_manager);
            manager.on_dialog_opened(&mut td, dialog_id);
            td.dialog_manager = manager;
        }
        let (total_count, dialog_ids) = {
            let mut manager = std::mem::take(&mut td.dialog_manager);
            let result = manager.recently_opened_dialogs.get_dialogs(&mut td, 10);
            td.dialog_manager = manager;
            result
        };
        assert_eq!(total_count, 1);
        assert_eq!(dialog_ids, vec![dialog_id]);

        {
            let mut manager = std::mem::take(&mut td.dialog_manager);
            manager.on_dialog_deleted(&mut td, dialog_id);
            td.dialog_manager = manager;
        }
        let (total_count, dialog_ids) = {
            let mut manager = std::mem::take(&mut td.dialog_manager);
            let result = manager.recently_opened_dialogs.get_dialogs(&mut td, 10);
            td.dialog_manager = manager;
            result
        };
        assert_eq!((total_count, dialog_ids), (0, Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_dialog_action_unneeded_treats_unknown_users_as_deleted() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_action_unneeded");
        {
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .set_my_id(&mut env, crate::user_manager::UserId::new(42));
        }

        // is_user_deleted(None) is true — an unknown user's dialog needs no
        // action, matching the C++ null-user ladder.
        assert!(is_dialog_action_unneeded(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7))
        ));
        // the current user's own dialog is always unneeded — but it is also
        // unknown here, so the deleted branch fires first either way.
        let my_id = td.user_manager.get_my_id();
        assert_eq!(my_id, crate::user_manager::UserId::new(42));
        assert!(is_dialog_action_unneeded(
            &mut td,
            DialogId::from_user_id(my_id)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_dialog_gates_answer_the_cpp_errors() {
        let mut td = test_td();
        // have_dialog_force stubs to the in-memory have_dialog_info, whose
        // chat/channel halves are hard `false` until MessagesManager lands —
        // so the C++ first gate answers before the type gate ("Only basic
        // group chats can be converted to supergroup") or the creator gate
        // below it can be reached.
        let (promise, slot) = capture_object_promise();
        crate::dialog_manager::migrate_dialog_to_megagroup(
            &mut td,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(5)),
            promise,
        );
        let status = captured_status(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Chat not found");

        // same for a basic group — the type gate is behind the same
        // have_dialog_force check and stays unreachable in the stub
        // inventory
        let (promise, slot) = capture_object_promise();
        crate::dialog_manager::migrate_dialog_to_megagroup(
            &mut td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(5)),
            promise,
        );
        let status = captured_status(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Chat not found");
    }

    #[test]
    fn delete_dialog_gates_and_cascades() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_delete_dialog");
        {
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager.on_get_user(
                &mut env,
                tdmtproto::telegram_api::User::user(tdmtproto::telegram_api::user {
                    id: 7,
                    flags: 1, // USER_ACCESS_HASH_MASK — a flagless user is a min user
                    access_hash: 123,
                    first_name: "Doomed".to_string(),
                    ..Default::default()
                }),
                "test",
            );
        }

        let (promise, slot) = capture_unit_promise();
        delete_dialog(&mut td, DialogId::new(0), promise);
        let status = captured_status(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Chat not found");

        // the user-dialog cascade is a net-blocked send; the seeded user
        // passes the have_dialog_force gate
        let (promise, slot) = capture_unit_promise();
        delete_dialog(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
            promise,
        );
        let status = captured_status(&slot);
        assert_eq!(status.code(), 500);
        assert_eq!(status.message(), "Network is not implemented yet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_dialog_title_gates() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_set_dialog_title");
        let user_dialog = DialogId::from_user_id(crate::user_manager::UserId::new(7));

        let (promise, slot) = capture_unit_promise();
        set_dialog_title(&mut td, user_dialog, "", promise);
        let status = captured_status(&slot);
        assert_eq!(status.message(), "Chat not found");

        // clear_dialogs persists through the binlog pmc; the title gate
        // still answers "Chat not found" for the unknown user afterwards
        {
            let mut manager = std::mem::take(&mut td.dialog_manager);
            manager.recently_found_dialogs.clear_dialogs(&mut td);
            td.dialog_manager = manager;
        }

        let (promise, slot) = capture_unit_promise();
        set_dialog_title(&mut td, user_dialog, "  ", promise);
        assert_eq!(captured_status(&slot).message(), "Chat not found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_dialog_permissions_gates() {
        let mut td = test_td();
        let user_dialog = DialogId::from_user_id(crate::user_manager::UserId::new(7));

        let (promise, slot) = capture_unit_promise();
        set_dialog_permissions(&mut td, user_dialog, None, promise);
        // have_dialog_force fails first — the C++ order keeps
        // "Chat not found" ahead of the null-permissions check.
        assert_eq!(captured_status(&slot).message(), "Chat not found");

        let (promise, slot) = capture_unit_promise();
        set_dialog_permissions(&mut td, DialogId::new(0), None, promise);
        let status = captured_status(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Invalid chat identifier specified");
    }

    #[test]
    fn toggle_dialog_has_protected_content_validates_the_request_message() {
        let mut td = test_td();
        // a user dialog passes the switch (no owner check), then the
        // is_request message-id gate: a local (non-server) id is invalid.
        let (promise, slot) = capture_unit_promise();
        toggle_dialog_has_protected_content(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
            Some(crate::chat_manager::MessageId::new(1 << 21)), // local id
            true,
            promise,
        );
        // have_dialog_force fails first for the unknown user
        assert_eq!(captured_status(&slot).message(), "Chat not found");

        let (promise, slot) = capture_unit_promise();
        toggle_dialog_has_protected_content(
            &mut td,
            DialogId::new(0),
            Some(crate::chat_manager::MessageId::new(1 << 21)),
            true,
            promise,
        );
        let status = captured_status(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "Invalid chat identifier specified");
    }

    #[test]
    fn can_report_dialog_matrix() {
        let mut td = test_td();
        // unknown user → deleted → not reportable
        assert!(!can_report_dialog(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7))
        ));
        // basic groups are never reportable; unloaded channels are (not a
        // creator); secret chats are not
        assert!(!can_report_dialog(
            &mut td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(5))
        ));
        assert!(can_report_dialog(
            &mut td,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(5))
        ));
        assert!(!can_report_dialog(
            &mut td,
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1))
        ));
    }

    #[test]
    fn report_dialog_gates() {
        let mut td = test_td();
        let (promise, slot) = {
            let slot = Arc::new(Mutex::new(None::<Result<crate::td_api::Object>>));
            let capture = Arc::clone(&slot);
            let promise =
                PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                    *capture.lock().unwrap() = Some(result);
                });
            (promise, slot)
        };
        crate::dialog_manager::report_dialog(&mut td, DialogId::new(0), "", &[], "", promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Invalid chat identifier specified")
            }
            _ => panic!("expected the access error"),
        }
    }

    #[test]
    fn get_blocked_dialogs_validates_arguments() {
        let mut td = test_td();
        let block_list = crate::td_api::BlockList::blockListMain(crate::td_api::blockListMain {});

        let (promise, slot) = capture_unit_promise();
        // Promise<()> vs the object promise — get_blocked_dialogs takes the
        // object form; reuse a unit-shaped assertion through the object slot
        let (object_promise, object_slot) = {
            let s = Arc::new(Mutex::new(None::<Result<crate::td_api::Object>>));
            let c = Arc::clone(&s);
            let p = PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                *c.lock().unwrap() = Some(result);
            });
            (p, s)
        };
        let _ = (promise, slot);
        get_blocked_dialogs(&mut td, Some(&block_list), -1, 10, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Parameter offset must be non-negative")
            }
            _ => panic!("expected the offset error"),
        }

        let (object_promise, object_slot) = {
            let s = Arc::new(Mutex::new(None::<Result<crate::td_api::Object>>));
            let c = Arc::clone(&s);
            let p = PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                *c.lock().unwrap() = Some(result);
            });
            (p, s)
        };
        get_blocked_dialogs(&mut td, Some(&block_list), 0, 0, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Parameter limit must be positive")
            }
            _ => panic!("expected the limit error"),
        }

        // a valid argument set reaches the net-blocked send
        let (object_promise, object_slot) = {
            let s = Arc::new(Mutex::new(None::<Result<crate::td_api::Object>>));
            let c = Arc::clone(&s);
            let p = PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                *c.lock().unwrap() = Some(result);
            });
            (p, s)
        };
        get_blocked_dialogs(&mut td, Some(&block_list), 0, 10, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Network is not implemented yet")
            }
            _ => panic!("expected the net-blocked error"),
        }

        assert_eq!(
            BlockListId::from_block_list(Some(&block_list)),
            Some(BlockListId::Main)
        );
    }

    #[test]
    fn on_get_blocked_dialogs_builds_message_senders() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "m6_blocked_senders");
        {
            let options = td.option_manager.as_mut().expect("user environment");
            let mut env = UserEnv {
                global: &mut td.global,
                options,
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager.on_get_user(
                &mut env,
                tdmtproto::telegram_api::User::user(tdmtproto::telegram_api::user {
                    id: 11,
                    flags: 1, // USER_ACCESS_HASH_MASK — a flagless user is a min user
                    access_hash: 123,
                    first_name: "Blocked".to_string(),
                    ..Default::default()
                }),
                "test",
            );
        }
        let (object_promise, object_slot) = {
            let s = Arc::new(Mutex::new(None::<Result<crate::td_api::Object>>));
            let c = Arc::clone(&s);
            let p = PromiseCreator::lambda_result(move |result: Result<crate::td_api::Object>| {
                *c.lock().unwrap() = Some(result);
            });
            (p, s)
        };
        let blocked = vec![tdmtproto::telegram_api::PeerBlocked {
            peer_id: Some(tdmtproto::telegram_api::Peer::peerUser(
                tdmtproto::telegram_api::peerUser { user_id: 11 },
            )),
            date: 0,
        }];
        on_get_blocked_dialogs(&mut td, 0, 1, 1, blocked, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Ok(object)) => match object {
                td_api::Object::messageSenders(senders) => {
                    assert_eq!(senders.total_count, 1);
                    assert_eq!(senders.senders.len(), 1);
                    assert!(matches!(
                        senders.senders[0],
                        td_api::MessageSender::messageSenderUser(_)
                    ));
                }
                other => panic!("expected messageSenders, got {other:?}"),
            },
            other => panic!("expected Ok, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dialog_log_event_records_round_trip() {
        let pinned = ToggleDialogPropertyOnServerLogEvent {
            dialog_id: DialogId::from_channel_id(crate::chat_manager::ChannelId::new(77)),
            value: true,
        };
        let mut data = Vec::new();
        pinned.store(&mut data);
        assert_eq!(
            ToggleDialogPropertyOnServerLogEvent::parse(&data).unwrap(),
            pinned
        );

        let blocked = ToggleDialogIsBlockedOnServerLogEvent {
            dialog_id: DialogId::from_user_id(crate::user_manager::UserId::new(9)),
            is_blocked: true,
            is_blocked_for_stories: false,
        };
        let mut data = Vec::new();
        blocked.store(&mut data);
        assert_eq!(
            ToggleDialogIsBlockedOnServerLogEvent::parse(&data).unwrap(),
            blocked
        );

        let topic = ToggleDialogTopicPropertyOnServerLogEvent {
            dialog_id: DialogId::from_user_id(crate::user_manager::UserId::new(9)),
            saved_messages_topic_id: 555,
            value: false,
        };
        let mut data = Vec::new();
        topic.store(&mut data);
        assert_eq!(
            ToggleDialogTopicPropertyOnServerLogEvent::parse(&data).unwrap(),
            topic
        );

        // the no-topic form parses back with the default topic id
        let mut data = Vec::new();
        ToggleDialogTopicPropertyOnServerLogEvent {
            saved_messages_topic_id: 0,
            ..topic
        }
        .store(&mut data);
        assert_eq!(
            ToggleDialogTopicPropertyOnServerLogEvent::parse(&data)
                .unwrap()
                .saved_messages_topic_id,
            0
        );

        let report_spam = ToggleDialogReportSpamStateOnServerLogEvent {
            dialog_id: DialogId::from_user_id(crate::user_manager::UserId::new(3)),
            is_spam_dialog: true,
        };
        let mut data = Vec::new();
        report_spam.store(&mut data);
        assert_eq!(
            ToggleDialogReportSpamStateOnServerLogEvent::parse(&data).unwrap(),
            report_spam
        );

        let reorder = ReorderPinnedDialogsOnServerLogEvent {
            folder_id: 1,
            dialog_ids: vec![
                DialogId::from_user_id(crate::user_manager::UserId::new(1)),
                DialogId::from_channel_id(crate::chat_manager::ChannelId::new(2)),
            ],
        };
        let mut data = Vec::new();
        reorder.store(&mut data);
        assert_eq!(
            ReorderPinnedDialogsOnServerLogEvent::parse(&data).unwrap(),
            reorder
        );

        // the pre-AddFolders (version < 23) form has no folder id
        let mut legacy = Vec::new();
        crate::store_parse::store_i32(&mut legacy, LOG_EVENT_ADD_FOLDERS_VERSION - 1);
        crate::store_parse::store_i32(&mut legacy, reorder.dialog_ids.len() as i32);
        for &dialog_id in &reorder.dialog_ids {
            crate::store_parse::store_i64(&mut legacy, dialog_id.get());
        }
        let parsed = ReorderPinnedDialogsOnServerLogEvent::parse(&legacy).unwrap();
        assert_eq!(parsed.folder_id, 0);
        assert_eq!(parsed.dialog_ids, reorder.dialog_ids);

        // a hostile element count is rejected
        let mut hostile = Vec::new();
        crate::store_parse::store_i32(&mut hostile, LOG_EVENT_ADD_FOLDERS_VERSION);
        crate::store_parse::store_i32(&mut hostile, 1_000_000);
        assert!(ReorderPinnedDialogsOnServerLogEvent::parse(&hostile).is_err());
    }

    #[test]
    fn check_dialog_username_local_halves() {
        let mut td = test_td();

        // an empty username from a user account is Ok
        let (object_promise, object_slot) = new_object_slot();
        check_dialog_username(&mut td, DialogId::new(0), "", false, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Ok(object)) => assert!(matches!(
                object,
                td_api::Object::checkChatUsernameResultOk(_)
            )),
            other => panic!("expected Ok result, got {other:?}"),
        }

        // an empty username from a bot is Invalid
        let (object_promise, object_slot) = new_object_slot();
        check_dialog_username(&mut td, DialogId::new(0), "", true, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Ok(object)) => assert!(matches!(
                object,
                td_api::Object::checkChatUsernameResultUsernameInvalid(_)
            )),
            other => panic!("expected Invalid, got {other:?}"),
        }

        // a reserved/invalid username is Invalid without a query
        let (object_promise, object_slot) = new_object_slot();
        check_dialog_username(&mut td, DialogId::new(0), "telegram", false, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Ok(object)) => assert!(matches!(
                object,
                td_api::Object::checkChatUsernameResultUsernameInvalid(_)
            )),
            other => panic!("expected Invalid, got {other:?}"),
        }

        // a 4-character username escapes the local invalid gate but reaches
        // the net-blocked check
        let (object_promise, object_slot) = new_object_slot();
        check_dialog_username(&mut td, DialogId::new(0), "abcd", false, object_promise);
        match object_slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Network is not implemented yet")
            }
            other => panic!("expected the net-blocked error, got {other:?}"),
        }

        // a basic group can only have an empty username
        let (object_promise, object_slot) = new_object_slot();
        check_dialog_username(
            &mut td,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(5)),
            "name",
            false,
            object_promise,
        );
        match object_slot.lock().unwrap().take() {
            Some(Result::Err(status)) => assert_eq!(status.message(), "Chat not found"),
            other => panic!("expected the chat gate, got {other:?}"),
        }
    }

    fn new_object_slot() -> (Promise<td_api::Object>, ObjectSlot) {
        let slot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = PromiseCreator::lambda_result(move |result: Result<td_api::Object>| {
            *capture.lock().unwrap() = Some(result);
        });
        (promise, slot)
    }

    #[test]
    fn search_public_dialog_gates_the_username() {
        let mut td = test_td();
        let (promise, slot) = capture_unit_promise();
        let dialog_id = search_public_dialog(&mut td, "@", false, promise);
        assert!(!dialog_id.is_valid());
        let status = captured_status(&slot);
        assert_eq!(status.code(), 200);
        assert_eq!(status.message(), "Username is invalid");
    }

    #[test]
    fn search_public_dialogs_short_prefix_branch_resolves_through_the_cache() {
        let mut td = test_td();
        // "gi" is a short-username prefix of "gif" — the cache is empty, so
        // the resolve is net-blocked and the search returns nothing.
        let (promise, slot) = capture_unit_promise();
        let found = search_public_dialogs(
            &mut td,
            "gi",
            Some(&td_api::SearchChatTypeFilter::searchChatTypeFilterBot(
                td_api::searchChatTypeFilterBot {},
            )),
            promise,
        );
        assert!(found.is_empty());
        assert_eq!(
            captured_status(&slot).message(),
            "Network is not implemented yet"
        );

        // a cached short username resolves and passes the filter checks
        td.dialog_manager
            .inaccessible_resolved_usernames
            .insert("gif".to_string(), DialogId::new(0));
        // the inaccessible form is not valid — search falls through to the
        // net-blocked resolve as well
        let (promise, slot) = capture_unit_promise();
        let found = search_public_dialogs(&mut td, "gi", None, promise);
        assert!(found.is_empty());
        assert_eq!(
            captured_status(&slot).message(),
            "Network is not implemented yet"
        );
    }

    #[test]
    fn search_public_dialogs_short_prefix_accepts_cached_non_user_dialogs() {
        let mut td = test_td();
        // a short-username match resolving to a CHANNEL must not evaluate
        // the user-contact check — `get_user_id` asserts `type == User`, and
        // the C++ short-circuits on the type first (cpp:3382-3383)
        let channel_dialog =
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(500_000_000));
        td.dialog_manager
            .on_dialog_usernames_received(channel_dialog, &["gif".to_string()], false);
        let (promise, slot) = capture_unit_promise();
        let found = search_public_dialogs(&mut td, "gi", None, promise);
        assert_eq!(found, vec![channel_dialog]);
        assert!(slot.lock().unwrap().is_some(), "the promise completed");
    }

    #[test]
    fn search_dialogs_on_server_gates_the_limit() {
        let mut td = test_td();
        let (promise, slot) = capture_unit_promise();
        let found = search_dialogs_on_server(&mut td, "query", None, -1, promise);
        assert!(found.is_empty());
        assert_eq!(
            captured_status(&slot).message(),
            "Limit must be non-negative"
        );

        // an empty query succeeds with no results
        let (promise, slot) = capture_unit_promise();
        let found = search_dialogs_on_server(&mut td, "", None, 10, promise);
        assert!(found.is_empty());
        assert!(slot.lock().unwrap().is_some());
    }

    #[test]
    fn can_delete_all_and_can_pin_answer_the_unloaded_results() {
        let mut td = test_td();
        let status = can_delete_all_dialog_messages_by_sender(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
        )
        .error()
        .clone();
        assert_eq!(
            status.message(),
            "All messages from a sender can be deleted only in supergroup chats"
        );

        let status = can_delete_all_dialog_messages_by_sender(
            &mut td,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(5)),
        )
        .error()
        .clone();
        assert_eq!(
            status.message(),
            "The method is available only in regular supergroup chats"
        );

        let status = can_pin_messages(
            &mut td,
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1)),
        )
        .error()
        .clone();
        assert_eq!(status.message(), "Secret chats can't have pinned messages");

        let status = can_pin_messages(
            &mut td,
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
        )
        .error()
        .clone();
        assert_eq!(status.message(), "Not enough rights");
    }

    #[test]
    fn toggle_server_mirrors_gate_secret_chats_and_persist_mirrors() {
        let mut td = test_td();
        let dir = init_td_db_for_test(&mut td, "toggle-mirrors");
        let channel_dialog =
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(500_000_000));

        // a secret chat never even creates the pinned/unread-mark mirrors
        let secret_dialog =
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1));
        assert_eq!(
            toggle_dialog_is_pinned_on_server(&mut td, secret_dialog, true, 0),
            0
        );
        assert_eq!(
            toggle_dialog_is_marked_as_unread_on_server(&mut td, secret_dialog, 0, true, 0),
            0
        );

        // non-secret dialogs save the mirror when the message database is on
        let pinned_log_event_id =
            toggle_dialog_is_pinned_on_server(&mut td, channel_dialog, true, 0);
        assert_ne!(pinned_log_event_id, 0);
        let unread_log_event_id =
            toggle_dialog_is_marked_as_unread_on_server(&mut td, channel_dialog, 0, true, 0);
        assert_ne!(unread_log_event_id, 0);

        // an explicit log event id is kept, not re-saved
        assert_eq!(
            toggle_dialog_is_pinned_on_server(&mut td, channel_dialog, false, pinned_log_event_id),
            pinned_log_event_id
        );

        let _ = unread_log_event_id;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn username_cache_round_trip_and_expiry() {
        let mut td = test_td();
        td.dialog_manager.on_dialog_usernames_received(
            DialogId::from_user_id(crate::user_manager::UserId::new(21)),
            &["SomeBody".to_string()],
            false,
        );
        assert_eq!(
            td.dialog_manager
                .get_resolved_dialog_by_username("some.body"),
            DialogId::from_user_id(crate::user_manager::UserId::new(21))
        );

        // the update erases the old spelling's cache entry
        td.dialog_manager.on_dialog_usernames_updated(
            DialogId::from_user_id(crate::user_manager::UserId::new(21)),
            &["SomeBody".to_string()],
            &[],
        );
        assert_eq!(
            td.dialog_manager
                .get_resolved_dialog_by_username("somebody"),
            DialogId::new(0)
        );

        // a database-loaded entry never expires: its expires_at is the
        // insertion-time clock (the C++ `Time::now() + 0`), not now + the
        // 86400-second cache ttl
        let inserted_before = time::now();
        td.dialog_manager.on_dialog_usernames_received(
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(3)),
            &["chan".to_string()],
            true,
        );
        let expires_at = td.dialog_manager.resolved_usernames["chan"].expires_at;
        assert!(expires_at >= inserted_before && expires_at <= time::now());
    }
}
