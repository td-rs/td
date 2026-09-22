//! The M.18.1/M.18.2 unit tests: the offline [`MessageQueryManagerEnv`]
//! stub (plain maps, like the M.16 split) plus the record-plane and
//! byte-exact logevent round-trip pins. Every logevent round-trip pins
//! every field; the byte pins cover the flag-block layouts, including the
//! deliberately empty `DeleteTopicHistory` flags word and the
//! pre-`AddKeyboardButtonFlags` sender-id migration.

use super::*;
use crate::store_parse::Parser;
use crate::store_parse::store_log_event_version;
use crate::user_manager::UserId;
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;

/// The offline [`MessageQueryManagerEnv`] stub — known users live in a
/// set, options in a map.
#[derive(Default)]
struct TestEnv {
    known_users: HashSet<i64>,
    options: HashMap<String, String>,
    unix_time: i32,
    is_bot: bool,
}

impl MessageQueryManagerEnv for TestEnv {
    fn auth_is_bot(&self) -> bool {
        self.is_bot
    }

    fn close_flag(&self) -> bool {
        false
    }

    fn unix_time(&self) -> i32 {
        self.unix_time
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options
            .get(name)
            .cloned()
            .unwrap_or_else(|| default_value.to_owned())
    }

    fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
        if self.known_users.contains(&user_id.get()) {
            user_id.get()
        } else {
            // the UserManager answers 0 for an unknown user
            0
        }
    }

    fn have_user_force(&mut self, user_id: UserId, _source: &str) -> bool {
        self.known_users.contains(&user_id.get())
    }
}

fn read_participant_date(user_id: i64, date: i32) -> telegram_api::ReadParticipantDate {
    telegram_api::ReadParticipantDate { user_id, date }
}

fn user_dialog(user_id: i64) -> DialogId {
    DialogId::from_user_id(UserId::new(user_id))
}

fn chat_dialog(chat_id: i64) -> DialogId {
    DialogId::from_chat_id(crate::chat_manager::ChatId::new(chat_id))
}

fn sample_message(id: i32, date: i32, user_id: i64) -> telegram_api::Message {
    telegram_api::Message::message(telegram_api::message {
        id,
        date,
        peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
            user_id,
        })),
        ..Default::default()
    })
}

#[test]
fn message_viewer_clamps_the_date_and_detects_emptiness() {
    let viewer = MessageViewer::new(UserId::new(42), -5);
    assert_eq!(viewer, MessageViewer::new(UserId::new(42), 0));
    assert!(!viewer.is_empty());
    assert!(MessageViewer::default().is_empty());

    let from_wire = MessageViewer::from_read_participant_date(&read_participant_date(43, 17));
    assert_eq!(from_wire, MessageViewer::new(UserId::new(43), 17));
}

#[test]
fn message_viewers_intake_drops_invalid_users_and_builds_objects() {
    let mut env = TestEnv {
        known_users: HashSet::from([42]),
        ..TestEnv::default()
    };
    let viewers = MessageViewers::from_read_participant_dates(vec![
        read_participant_date(42, 100),
        read_participant_date(0, 101),  // invalid: zero
        read_participant_date(-7, 102), // invalid: negative
    ]);
    assert_eq!(viewers.get_user_ids(), vec![UserId::new(42)]);

    let object = viewers.get_message_viewers_object(&mut env);
    assert_eq!(object.viewers.len(), 1);
    assert_eq!(object.viewers[0].user_id, 42);
    assert_eq!(object.viewers[0].view_date, 100);

    // an unknown user still builds, answering the 0 id like the C++
    let unknown = MessageViewers::from_read_participant_dates(vec![read_participant_date(43, 5)]);
    let object = unknown.get_message_viewers_object(&mut env);
    assert_eq!(object.viewers[0].user_id, 0);
    assert_eq!(object.viewers[0].view_date, 5);
}

#[test]
fn affected_history_folds_the_offset_into_is_final() {
    let history = AffectedHistory::from_affected_history(&telegram_api::messages_AffectedHistory {
        pts: 10,
        pts_count: 2,
        offset: 0,
    });
    assert_eq!(
        history,
        AffectedHistory {
            pts: 10,
            pts_count: 2,
            is_final: true
        }
    );

    let history = AffectedHistory::from_affected_found_messages(
        &telegram_api::messages_AffectedFoundMessages {
            pts: 11,
            pts_count: 1,
            offset: 5,
            messages: vec![1, 2, 3],
        },
    );
    assert_eq!(
        history,
        AffectedHistory {
            pts: 11,
            pts_count: 1,
            is_final: false
        }
    );
}

#[test]
fn the_amount_clamp_matches_ton_and_star_count() {
    let max = 1i64 << 51;
    assert_eq!(get_amount_count(5, false), 5);
    assert_eq!(get_amount_count(-5, false), 0);
    assert_eq!(get_amount_count(-5, true), -5);
    assert_eq!(get_amount_count(max + 1, false), max);
    assert_eq!(get_amount_count(-max - 1, true), -max);
}

#[test]
fn emoji_game_info_intake_rejects_bad_shapes() {
    let good = telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
        telegram_api::messages_emojiGameDiceInfo {
            flags: 0,
            game_hash: "hash".to_owned(),
            prev_stake: 1500000000,
            current_streak: 2,
            params: vec![1, 2, 3, 4, 5, 6, 7],
            plays_left: 0,
        },
    );
    let info = EmojiGameInfo::from_wire(&good);
    assert_eq!(
        info,
        EmojiGameInfo {
            game_hash: "hash".to_owned(),
            prev_stake: 1500000000,
            current_streak: 2,
            params: vec![1, 2, 3, 4, 5, 6, 7],
        }
    );

    // the unavailable constructor resets to the default record
    let unavailable = telegram_api::messages_EmojiGameInfo::messages_emojiGameUnavailable(
        telegram_api::messages_emojiGameUnavailable {},
    );
    assert_eq!(
        EmojiGameInfo::from_wire(&unavailable),
        EmojiGameInfo::default()
    );

    // each bad-shape branch falls back to the default
    let bad = |game_hash: &str, prev_stake: i64, current_streak: i32, params: Vec<i32>| {
        telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
            telegram_api::messages_emojiGameDiceInfo {
                flags: 0,
                game_hash: game_hash.to_owned(),
                prev_stake,
                current_streak,
                params,
                plays_left: 0,
            },
        )
    };
    assert_eq!(
        EmojiGameInfo::from_wire(&bad("", 1, 0, vec![1; 7])),
        EmojiGameInfo::default()
    );
    assert_eq!(
        EmojiGameInfo::from_wire(&bad("h", -1, 0, vec![1; 7])),
        EmojiGameInfo::default()
    );
    assert_eq!(
        EmojiGameInfo::from_wire(&bad("h", 1, 3, vec![1; 7])),
        EmojiGameInfo::default()
    );
    assert_eq!(
        EmojiGameInfo::from_wire(&bad("h", 1, 0, vec![1; 6])),
        EmojiGameInfo::default()
    );
    assert_eq!(
        EmojiGameInfo::from_wire(&bad("h", 1, 0, vec![-1, 1, 1, 1, 1, 1, 1])),
        EmojiGameInfo::default()
    );
}

#[test]
fn emoji_game_info_builds_the_stake_dice_state() {
    let mut env = TestEnv::default();
    env.options.insert(
        "ton_stakedice_stake_suggested_amounts".to_owned(),
        "1,2,3".to_owned(),
    );
    let info = EmojiGameInfo {
        game_hash: "hash".to_owned(),
        prev_stake: 2500000000,
        current_streak: 1,
        params: vec![10, 20, 30, 40, 50, 60, 70],
    };
    let state = info.get_stake_dice_state_object(&env);
    assert_eq!(state.state_hash, "hash");
    assert_eq!(state.stake_gram_amount, 2500000000);
    assert_eq!(state.suggested_stake_gram_amounts, vec![1, 2, 3]);
    assert_eq!(state.current_streak, 1);
    assert_eq!(state.prize_per_mille, vec![10, 20, 30, 40, 50, 60]);
    assert_eq!(state.streak_prize_per_mille, 70);

    // the update wraps the same state
    let td_api::Update::updateStakeDiceState(update) =
        info.get_update_stake_dice_state_object(&env)
    else {
        panic!("wrong update constructor");
    };
    assert_eq!(update.state.unwrap(), state);

    // the empty record builds the default state without reading options
    let state = EmojiGameInfo::default().get_stake_dice_state_object(&env);
    assert_eq!(state, td_api::StakeDiceState::default());
}

#[test]
fn search_posts_flood_builds_the_limits_object() {
    let env = TestEnv {
        unix_time: 1000,
        ..TestEnv::default()
    };
    let flood = SearchPostsFlood::from_wire(&telegram_api::SearchPostsFlood {
        flags: 1, // query_is_free
        query_is_free: true,
        total_daily: 100,
        remains: 3,
        wait_till: 1600,
        stars_amount: 50,
    });
    assert!(flood.is_free());
    assert_eq!(
        flood.get_public_post_search_limits_object(&env),
        td_api::PublicPostSearchLimits {
            daily_free_query_count: 100,
            remaining_free_query_count: 3,
            next_free_query_in: 600,
            star_count: 50,
            is_current_query_free: true,
        }
    );

    // the next-free counter clamps at zero and the star count clamps at 2^51
    let exhausted = SearchPostsFlood::from_wire(&telegram_api::SearchPostsFlood {
        flags: 0,
        query_is_free: false,
        total_daily: 100,
        remains: 0,
        wait_till: 5,
        stars_amount: (1i64 << 51) + 5,
    });
    assert!(!exhausted.is_free());
    assert_eq!(
        exhausted.get_public_post_search_limits_object(&env),
        td_api::PublicPostSearchLimits {
            daily_free_query_count: 100,
            remaining_free_query_count: 0,
            next_free_query_in: 0,
            star_count: 1i64 << 51,
            is_current_query_free: false,
        }
    );
}

#[test]
fn message_search_offset_round_trips_and_validates() {
    // the valid form round-trips through the wire string
    let offset = MessageSearchOffset::from_offset_string("100,777,7").unwrap();
    assert_eq!(offset.to_offset_string(), "100,777,7");

    // the empty offset is the unbounded form
    let unbounded = MessageSearchOffset::from_offset_string("").unwrap();
    assert_eq!(unbounded.to_offset_string(), "2147483647,0,0");

    // the invalid bands
    for bad in [
        "1,2",
        "1,2,3,4",
        "x,777,7",
        "100,777,x",
        "0,777,7",
        "100,0,7",
        "100,777,0",
    ] {
        let error = MessageSearchOffset::from_offset_string(bad).unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Invalid offset specified");
    }
}

#[test]
fn message_search_offset_updates_from_messages() {
    let mut offset = MessageSearchOffset::default();
    // an empty message carries no position and is skipped
    offset.update_from_message(&telegram_api::Message::messageEmpty(
        telegram_api::messageEmpty {
            flags: 0,
            ..Default::default()
        },
    ));
    assert_eq!(offset.to_offset_string(), "0,0,0");

    // a usable message sets the position; a later one overwrites it
    offset.update_from_message(&sample_message(5, 1000, 777));
    assert_eq!(offset.to_offset_string(), "1000,777,5");
    offset.update_from_message(&sample_message(6, 1001, 777));
    assert_eq!(offset.to_offset_string(), "1001,777,6");
}

/// The logevent round-trip helper: version-prefix + store + parse, with
/// the trailing-bytes pin (`fetch_end`) to catch framing drift.
fn round_trip<T: Default + PartialEq + std::fmt::Debug>(
    log_event: T,
    store: fn(&T, &mut Vec<u8>),
    parse: fn(&mut Parser<'_>) -> Result<T, &'static str>,
    expected_len: Option<usize>,
) -> T {
    let mut blob = Vec::new();
    store_log_event_version(&mut blob);
    store(&log_event, &mut blob);
    if let Some(expected_len) = expected_len {
        assert_eq!(blob.len(), 4 + expected_len, "byte-length pin failed");
    }
    let mut parser = Parser::new_log_event(&blob).unwrap();
    let parsed = parse(&mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed, log_event);
    parsed
}

#[test]
fn block_message_sender_from_replies_log_event_round_trips() {
    round_trip(
        BlockMessageSenderFromRepliesOnServerLogEvent {
            message_id: MessageId::from_server_message_id(ServerMessageId::new(9)),
            delete_message: true,
            delete_all_messages: false,
            report_spam: true,
        },
        |value, out| value.store(out),
        BlockMessageSenderFromRepliesOnServerLogEvent::parse,
        // flags word + message id
        Some(4 + 8),
    );
}

#[test]
fn delete_all_call_messages_log_event_round_trips() {
    round_trip(
        DeleteAllCallMessagesOnServerLogEvent { revoke: true },
        |value, out| value.store(out),
        DeleteAllCallMessagesOnServerLogEvent::parse,
        Some(4),
    );
}

#[test]
fn delete_all_channel_messages_from_sender_log_event_round_trips() {
    // the current form stores the sender as a dialog id
    round_trip(
        DeleteAllChannelMessagesFromSenderOnServerLogEvent {
            channel_id: ChannelId::new(1234),
            sender_dialog_id: chat_dialog(500),
        },
        |value, out| value.store(out),
        DeleteAllChannelMessagesFromSenderOnServerLogEvent::parse,
        Some(8 + 8),
    );

    // the pre-AddKeyboardButtonFlags form stores a bare user id
    let mut blob = Vec::new();
    store_parse::store_i32(&mut blob, 34); // Version 34 < AddKeyboardButtonFlags
    store_parse::store_i64(&mut blob, 1234);
    store_parse::store_i64(&mut blob, 42); // the user id
    let mut parser = Parser::new_log_event(&blob).unwrap();
    let parsed = DeleteAllChannelMessagesFromSenderOnServerLogEvent::parse(&mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.channel_id, ChannelId::new(1234));
    assert_eq!(parsed.sender_dialog_id, user_dialog(42));
}

#[test]
fn delete_dialog_history_log_event_round_trips() {
    round_trip(
        DeleteDialogHistoryOnServerLogEvent {
            dialog_id: chat_dialog(600),
            max_message_id: MessageId::from_server_message_id(ServerMessageId::new(11)),
            remove_from_dialog_list: true,
            revoke: false,
        },
        |value, out| value.store(out),
        DeleteDialogHistoryOnServerLogEvent::parse,
        Some(4 + 8 + 8),
    );
}

#[test]
fn delete_dialog_messages_by_date_log_event_round_trips() {
    round_trip(
        DeleteDialogMessagesByDateOnServerLogEvent {
            dialog_id: user_dialog(43),
            min_date: 1376438400,
            max_date: 1600000000,
            revoke: true,
        },
        |value, out| value.store(out),
        DeleteDialogMessagesByDateOnServerLogEvent::parse,
        Some(4 + 8 + 4 + 4),
    );
}

#[test]
fn delete_messages_log_event_round_trips() {
    round_trip(
        DeleteMessagesOnServerLogEvent {
            dialog_id: user_dialog(44),
            message_ids: vec![
                MessageId::from_server_message_id(ServerMessageId::new(1)),
                MessageId::from_server_message_id(ServerMessageId::new(2)),
            ],
            revoke: true,
        },
        |value, out| value.store(out),
        DeleteMessagesOnServerLogEvent::parse,
        Some(4 + 8 + 4 + 2 * 8),
    );
}

#[test]
fn delete_scheduled_messages_log_event_round_trips() {
    // no flag block on this one
    round_trip(
        DeleteScheduledMessagesOnServerLogEvent {
            dialog_id: user_dialog(45),
            message_ids: vec![MessageId::from_server_message_id(ServerMessageId::new(3))],
        },
        |value, out| value.store(out),
        DeleteScheduledMessagesOnServerLogEvent::parse,
        Some(8 + 4 + 8),
    );
}

#[test]
fn delete_ephemeral_message_log_event_round_trips() {
    round_trip(
        DeleteEphemeralMessageOnServerLogEvent {
            dialog_id: user_dialog(46),
            receiver_dialog_id: user_dialog(47),
            ephemeral_message_id: 12,
        },
        |value, out| value.store(out),
        DeleteEphemeralMessageOnServerLogEvent::parse,
        Some(8 + 8 + 4),
    );
}

#[test]
fn delete_topic_history_log_event_round_trips() {
    round_trip(
        DeleteTopicHistoryOnServerLogEvent {
            dialog_id: user_dialog(48),
            forum_topic_id: crate::files::ForumTopicId::new(5),
        },
        |value, out| value.store(out),
        DeleteTopicHistoryOnServerLogEvent::parse,
        // the empty flags word is still stored
        Some(4 + 8 + 4),
    );
}

#[test]
fn read_all_dialog_mentions_log_event_round_trips() {
    round_trip(
        ReadAllDialogMentionsOnServerLogEvent {
            dialog_id: user_dialog(49),
        },
        |value, out| value.store(out),
        ReadAllDialogMentionsOnServerLogEvent::parse,
        Some(8),
    );
}

#[test]
fn read_all_dialog_reactions_log_event_round_trips() {
    round_trip(
        ReadAllDialogReactionsOnServerLogEvent {
            dialog_id: chat_dialog(700),
        },
        |value, out| value.store(out),
        ReadAllDialogReactionsOnServerLogEvent::parse,
        Some(8),
    );
}

#[test]
fn read_all_poll_votes_log_event_round_trips() {
    // with a valid topic id
    round_trip(
        ReadAllPollVotesOnServerLogEvent {
            dialog_id: user_dialog(50),
            forum_topic_id: crate::files::ForumTopicId::new(6),
        },
        |value, out| value.store(out),
        ReadAllPollVotesOnServerLogEvent::parse,
        Some(4 + 8 + 4),
    );

    // without one — the field is omitted from the bytes
    round_trip(
        ReadAllPollVotesOnServerLogEvent {
            dialog_id: user_dialog(50),
            forum_topic_id: crate::files::ForumTopicId::new(0),
        },
        |value, out| value.store(out),
        ReadAllPollVotesOnServerLogEvent::parse,
        Some(4 + 8),
    );
}

#[test]
fn read_message_contents_log_event_round_trips() {
    round_trip(
        ReadMessageContentsOnServerLogEvent {
            dialog_id: user_dialog(51),
            message_ids: vec![
                MessageId::from_server_message_id(ServerMessageId::new(7)),
                MessageId::from_server_message_id(ServerMessageId::new(8)),
                MessageId::from_server_message_id(ServerMessageId::new(9)),
            ],
        },
        |value, out| value.store(out),
        ReadMessageContentsOnServerLogEvent::parse,
        Some(8 + 4 + 3 * 8),
    );
}

#[test]
fn unpin_all_dialog_messages_log_event_round_trips() {
    round_trip(
        UnpinAllDialogMessagesOnServerLogEvent {
            dialog_id: user_dialog(52),
        },
        |value, out| value.store(out),
        UnpinAllDialogMessagesOnServerLogEvent::parse,
        Some(8),
    );
}

#[test]
fn the_log_event_type_names_are_the_handler_identities() {
    assert_eq!(
        MessageQueryLogEventType::DeleteMessagesOnServer.to_string(),
        "DeleteMessagesOnServer"
    );
    assert_eq!(
        MessageQueryLogEventType::BlockMessageSenderFromRepliesOnServer.to_string(),
        "BlockMessageSenderFromRepliesOnServer"
    );
}
