//! The S76 walking-skeleton conversions: `telegram_api` objects → `td_api`
//! objects for the surfaces the gate exercises (the self bot user, the
//! phone peer user, the private chat, plain-text messages). The full
//! UserManager/MessagesManager conversions land with the Track I wave;
//! these follow the C++ emission points the S76 harness pins:
//! `UserManager::get_user_object` (UserManager.cpp:10297-10340),
//! `get_user_status_object` (10227-10258), `MessagesManager::get_message_object`
//! (20732-20824) and `DialogManager::get_chat_object` for the fresh
//! private dialog — the exact field tables the live C++ recorder emits.

use tdmtproto::telegram_api;

use crate::td_api;

/// `UserManager::get_user_object` — the §1 field table. The usernames key
/// is omitted when the account has none (the C++ null-object omission);
/// bots take the `userTypeBot` type with the reference flag mapping.
pub fn user_object(user: &telegram_api::user, is_self: bool) -> td_api::User {
    let usernames = (!user.username.is_empty()).then(|| td_api::Usernames {
        active_usernames: vec![user.username.clone()],
        editable_username: user.username.clone(),
        ..Default::default()
    });
    let language_code = if user.lang_code.is_empty() {
        "en".to_string()
    } else {
        user.lang_code.clone()
    };
    let r#type = if user.bot {
        td_api::UserType::userTypeBot(td_api::userTypeBot {
            can_be_edited: !user.bot_nochats,
            can_join_groups: !user.bot_nochats,
            can_manage_bots: false,
            is_inline: !user.bot_inline_placeholder.is_empty(),
            inline_query_placeholder: user.bot_inline_placeholder.clone(),
            ..Default::default()
        })
    } else {
        td_api::UserType::userTypeRegular(td_api::userTypeRegular)
    };
    let peer_color = |color: &Option<telegram_api::PeerColor>| match color {
        Some(telegram_api::PeerColor::peerColor(c)) => Some((c.color, c.background_emoji_id)),
        // `peerColorCollectible` packs per-icon colors — take the first
        // (the walking skeleton sees no collectible peers).
        Some(telegram_api::PeerColor::peerColorCollectible(c)) => {
            c.colors.first().map(|icon| (*icon, c.background_emoji_id))
        }
        _ => None,
    };
    // `get_user_accent_color_id`: the custom color when chosen, else the
    // deterministic id-based default (id % 7 — the C++ hash).
    let (accent_color_id, background_custom_emoji_id) =
        peer_color(&user.color).unwrap_or(((user.id % 7) as i32, 0));
    let profile_accent_color_id = peer_color(&user.profile_color).map_or(-1, |(color, _)| color);
    td_api::User {
        id: user.id,
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        usernames,
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
        accent_color_id,
        background_custom_emoji_id,
        profile_accent_color_id,
        is_contact: user.contact,
        is_mutual_contact: user.mutual_contact,
        is_premium: user.premium,
        is_support: user.support,
        have_access: true,
        r#type: Some(r#type),
        language_code,
        ..Default::default()
    }
}

/// `UserManager::get_user_status_object` (10227-10258) + the self-user
/// clamp (the self bot reads `userStatusOnline` with the always-online
/// `expires`; the C++ maps `userStatusRecently` to Empty for non-contacts).
pub fn user_status_object(status: &telegram_api::UserStatus, is_self: bool) -> td_api::UserStatus {
    use td_api::UserStatus as S;
    use telegram_api::UserStatus as W;
    match status {
        W::userStatusEmpty(_) if is_self => S::userStatusOnline(td_api::userStatusOnline {
            expires: 2147483647,
        }),
        W::userStatusEmpty(_) => S::userStatusEmpty(td_api::userStatusEmpty),
        W::userStatusOnline(m) => {
            S::userStatusOnline(td_api::userStatusOnline { expires: m.expires })
        }
        W::userStatusOffline(m) => S::userStatusOffline(td_api::userStatusOffline {
            was_online: m.was_online,
        }),
        W::userStatusRecently(_) => S::userStatusEmpty(td_api::userStatusEmpty),
        W::userStatusLastWeek(_) => S::userStatusLastWeek(td_api::userStatusLastWeek::default()),
        W::userStatusLastMonth(_) => S::userStatusLastMonth(td_api::userStatusLastMonth::default()),
    }
}

/// The message fields the plain-text walking-skeleton path needs.
pub struct MessageSpec {
    pub id: i64,
    pub chat_id: i64,
    pub sender_user_id: i64,
    pub date: i32,
    pub text: String,
    /// `messageSendingStatePending(0)` when set; null otherwise.
    pub pending: bool,
    /// The received messages carry `is_outgoing = false`; the sent ones
    /// force it true (the C++ MessagesManager semantics).
    pub is_outgoing: bool,
}

/// `MessagesManager::get_message_object` for a text message (§3): the
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
        is_outgoing: spec.is_outgoing,
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

/// `DialogManager::get_chat_object` for the fresh private dialog the live
/// recorder pins: `chatTypePrivate`, the peer-name title, the private-chat
/// permission table, empty positions and the default tail (no
/// `last_message` — the fresh dialog carries none).
pub fn chat_object(user: &td_api::User) -> td_api::Chat {
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
        accent_color_id: user.accent_color_id,
        profile_accent_color_id: user.profile_accent_color_id,
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
            can_react_to_messages: true,
            can_pin_messages: true,
            ..Default::default()
        }),
        positions: Vec::new(),
        chat_lists: Vec::new(),
        last_message: None,
        unread_count: 0,
        last_read_inbox_message_id: 0,
        last_read_outbox_message_id: 0,
        notification_settings: Some(td_api::ChatNotificationSettings {
            use_default_mute_for: true,
            use_default_sound: true,
            sound_id: -1,
            use_default_show_preview: true,
            show_preview: true,
            use_default_mute_stories: true,
            use_default_story_sound: true,
            story_sound_id: -1,
            use_default_show_story_poster: true,
            show_story_poster: true,
            use_default_disable_pinned_message_notifications: true,
            use_default_disable_mention_notifications: true,
            ..Default::default()
        }),
        available_reactions: Some(td_api::ChatAvailableReactions::chatAvailableReactionsAll(
            td_api::chatAvailableReactionsAll {
                max_reaction_count: 11,
            },
        )),
        message_auto_delete_time: 0,
        video_chat: Some(td_api::VideoChat::default()),
        reply_markup_message_id: 0,
        client_data: String::new(),
        ..Default::default()
    }
}
