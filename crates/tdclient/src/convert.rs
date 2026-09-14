//! The S76 walking-skeleton conversions: `telegram_api` objects → `td_api`
//! objects for the surfaces the gate exercises (the self user, a text
//! message in Saved Messages, the Saved Messages chat). The full
//! UserManager/MessagesManager conversions land with the Track I wave;
//! these follow the C++ emission points the S76 harness pins:
//! `UserManager::get_user_object` (UserManager.cpp:10297-10340),
//! `get_user_status_object` (10227-10258), and
//! `MessagesManager::get_message_object` (20732-20824) for the text-only
//! Saved Messages shape (internal `is_outgoing` is false there; the td_api
//! object forces it true — MessagesManager.cpp:20746-20756).

use tdmtproto::telegram_api;

use crate::td_api;

/// `UserManager::get_user_object` — the §1 field table (a plain account:
/// no photo, no usernames, not premium).
pub fn user_object(user: &telegram_api::user, is_self: bool) -> td_api::User {
    td_api::User {
        id: user.id,
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        usernames: Some(td_api::Usernames::default()),
        phone_number: user.phone.clone(),
        status: Some(user_status_object(
            user.status
                .as_ref()
                .unwrap_or(&telegram_api::UserStatus::userStatusEmpty(
                    telegram_api::userStatusEmpty,
                )),
            is_self,
        )),
        profile_photo: None,
        accent_color_id: if is_self { -1 } else { 0 },
        is_contact: is_self,
        is_mutual_contact: is_self,
        is_premium: user.premium,
        is_support: user.support,
        have_access: true,
        r#type: Some(td_api::UserType::userTypeRegular(td_api::userTypeRegular)),
        ..Default::default()
    }
}

/// `UserManager::get_user_status_object` (10227-10258) + the self-user
/// clamp (Online→expires, Offline→was_online; negatives ignored for self).
pub fn user_status_object(status: &telegram_api::UserStatus, _is_self: bool) -> td_api::UserStatus {
    use td_api::UserStatus as S;
    use telegram_api::UserStatus as W;
    match status {
        W::userStatusEmpty(_) => S::userStatusEmpty(td_api::userStatusEmpty),
        W::userStatusOnline(m) => {
            S::userStatusOnline(td_api::userStatusOnline { expires: m.expires })
        }
        W::userStatusOffline(m) => S::userStatusOffline(td_api::userStatusOffline {
            was_online: m.was_online,
        }),
        W::userStatusRecently(_) => S::userStatusRecently(td_api::userStatusRecently::default()),
        W::userStatusLastWeek(_) => S::userStatusLastWeek(td_api::userStatusLastWeek::default()),
        W::userStatusLastMonth(_) => S::userStatusLastMonth(td_api::userStatusLastMonth::default()),
    }
}

/// The message fields the text-only Saved Messages path needs.
pub struct MessageSpec {
    pub id: i64,
    pub chat_id: i64,
    pub sender_user_id: i64,
    pub date: i32,
    pub text: String,
    /// `messageSendingStatePending(0)` when set; null otherwise.
    pub pending: bool,
}

/// `MessagesManager::get_message_object` for a text message (§3): the
/// forced `is_outgoing = true` of the Saved Messages dialog, the
/// `messageText` content, and the zero/empty/default tail.
pub fn message_object(spec: &MessageSpec) -> td_api::Message {
    td_api::Message {
        id: spec.id,
        sender_id: Some(td_api::MessageSender::messageSenderUser(
            td_api::messageSenderUser {
                user_id: spec.sender_user_id,
            },
        )),
        chat_id: spec.chat_id,
        sending_state: if spec.pending {
            Some(td_api::MessageSendingState::messageSendingStatePending(
                td_api::messageSendingStatePending { sending_id: 0 },
            ))
        } else {
            None
        },
        is_outgoing: true,
        can_be_saved: true,
        has_timestamped_media: true,
        date: spec.date,
        content: Some(td_api::MessageContent::messageText(td_api::messageText {
            text: Some(td_api::FormattedText {
                text: spec.text.clone(),
                entities: Vec::new(),
            }),
            link_preview: None,
            link_preview_options: None,
        })),
        ..Default::default()
    }
}

/// `DialogManager::get_chat_object` for the Saved Messages chat (§5):
/// `chatTypePrivate(self)`, the own-name title, view_as_topics=true
/// (`view_as_messages` in this scheme), and the fresh-dialog read
/// positions.
pub fn chat_object(
    user: &td_api::User,
    last_message: Option<td_api::Message>,
    last_read_inbox: i64,
) -> td_api::Chat {
    let mut title = user.first_name.clone();
    if !user.last_name.is_empty() {
        if !title.is_empty() {
            title.push(' ');
        }
        title.push_str(&user.last_name);
    }
    td_api::Chat {
        id: user.id,
        r#type: Some(td_api::ChatType::chatTypePrivate(td_api::chatTypePrivate {
            user_id: user.id,
        })),
        title,
        permissions: Some(td_api::ChatPermissions {
            can_send_basic_messages: true,
            can_send_audios: true,
            can_send_documents: true,
            can_send_photos: true,
            can_send_videos: true,
            can_send_video_notes: true,
            can_send_voice_notes: true,
            can_send_polls: true,
            can_send_other_messages: true,
            can_add_link_previews: true,
            can_change_info: true,
            can_invite_users: true,
            can_pin_messages: true,
            can_create_topics: true,
            ..Default::default()
        }),
        last_message,
        positions: vec![td_api::ChatPosition {
            list: Some(td_api::ChatList::chatListMain(td_api::chatListMain)),
            order: 0,
            is_pinned: false,
            source: None,
        }],
        view_as_topics: true,
        unread_count: 0,
        last_read_inbox_message_id: last_read_inbox,
        last_read_outbox_message_id: last_read_inbox,
        ..Default::default()
    }
}
