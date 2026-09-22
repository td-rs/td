//! The M.19.1 unit tests: the poll value plane (PollId, the quiz checker,
//! the vote-percentage machine) and the byte-exact `PollOption`/`Poll`/
//! log-event store/parse round-trips. The store/parse env delegates the
//! FileId codec to the real [`crate::files::FileManager`] — the same split
//! the td.rs wiring uses — and records the `[blocked: M.9 integration]`
//! media seam by name.

use super::*;
use crate::chat_manager::{ChannelId, MessageId, MinChannel};
use crate::dialog_id::MessageFullId;
use crate::emoji_status::CustomEmojiId;
use crate::files::FileManager;
use crate::message_entity::{MessageEntity, Type};
use crate::peer_color::AccentColorId;
use crate::store_parse::{Parser, store_i32, store_log_event_version, store_string};
use crate::user_manager::{FileId, UserId};
use tdutils::status::Status;

/// The store/parse env of the fixture: the real FileId codec, an empty
/// media seam (no poll record on this line carries media — the intake
/// paths are the M.9/M.10 blocked band).
struct TestStoreEnv {
    files: FileManager,
    stored_media: Vec<i32>,
}

impl Default for TestStoreEnv {
    fn default() -> TestStoreEnv {
        TestStoreEnv {
            files: FileManager::new(),
            stored_media: Vec::new(),
        }
    }
}

impl crate::photo::DialogPhotoStoreEnv for TestStoreEnv {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        self.files.store_file(file_id, out);
    }

    fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
        self.files.parse_file(parser)
    }
}

impl PollStoreEnv for TestStoreEnv {
    fn store_poll_media(&mut self, media_type: i32, _out: &mut Vec<u8>) {
        self.stored_media.push(media_type);
    }

    fn parse_poll_media(&mut self, _parser: &mut Parser<'_>) -> Result<i32, &'static str> {
        Err("Poll media parse — [blocked: M.9 integration]")
    }
}

fn custom_emoji_entity(offset: i32, length: i32, document_id: i64) -> MessageEntity {
    MessageEntity::custom_emoji(offset, length, CustomEmojiId::new(document_id))
}

fn italic_entity(offset: i32, length: i32) -> MessageEntity {
    MessageEntity::new(Type::Italic, offset, length)
}

// ---------------------------------------------------------------------------
// PollId + value helpers
// ---------------------------------------------------------------------------

#[test]
fn local_poll_id_bands() {
    // PollManager.cpp:449-451 — negative but not INT32_MIN.
    assert!(!is_local_poll_id(PollId::new(0)));
    assert!(!is_local_poll_id(PollId::new(1)));
    assert!(!is_local_poll_id(PollId::new(-1i64 << 62)));
    assert!(is_local_poll_id(PollId::new(-1)));
    assert!(is_local_poll_id(PollId::new(i32::MIN as i64 + 1)));
    assert!(!is_local_poll_id(PollId::new(i32::MIN as i64)));
    assert!(!is_local_poll_id(PollId::new(i32::MIN as i64 - 1)));
}

#[test]
fn quiz_correct_option_ids_checks() {
    // PollManager.cpp:453-469.
    assert_eq!(
        check_quiz_correct_option_ids(&[], 3, false)
            .unwrap_err()
            .code(),
        400
    );
    assert_eq!(
        check_quiz_correct_option_ids(&[], 3, true)
            .err()
            .map(|s| s.message().to_owned()),
        None
    );
    assert!(check_quiz_correct_option_ids(&[0, 1, 2], 3, false).is_ok());
    assert_eq!(
        check_quiz_correct_option_ids(&[2, 1], 3, true)
            .unwrap_err()
            .message(),
        "Correct quiz option list must be increasing"
    );
    assert_eq!(
        check_quiz_correct_option_ids(&[1, 1], 3, true)
            .unwrap_err()
            .message(),
        "Correct quiz option list must be increasing"
    );
    assert_eq!(
        check_quiz_correct_option_ids(&[-1], 3, true)
            .unwrap_err()
            .message(),
        "Wrong quiz correct_option_id"
    );
    assert_eq!(
        check_quiz_correct_option_ids(&[3], 3, true)
            .unwrap_err()
            .message(),
        "Wrong quiz correct_option_id"
    );
    assert!(check_quiz_correct_option_ids(&[0, 2], 3, true).is_ok());
}

#[test]
fn vote_percentage_nearest_rounding() {
    // The "just round to the nearest" band: the option counts don't sum to
    // the total (PollManager.cpp:627-646). The total clamps DOWN to the
    // sum first.
    assert_eq!(get_vote_percentage(&[1, 1, 2], 5), vec![25, 25, 50]);
    assert_eq!(get_vote_percentage(&[1, 1, 2], 6), vec![25, 25, 50]);
    assert_eq!(get_vote_percentage(&[0, 0], 5), vec![0, 0]);
}

#[test]
fn vote_percentage_equal_options_stay_equal() {
    // The exact-sum band (PollManager.cpp:648-721).
    // 1/3 tie: 25/75, nothing to top up.
    assert_eq!(get_vote_percentage(&[1, 3], 4), vec![25, 75]);
    // two-way 1/1: exactly 50/50.
    assert_eq!(get_vote_percentage(&[1, 1], 2), vec![50, 50]);
    // 1/1/2 (four voters): 25/25/50, no gap.
    assert_eq!(get_vote_percentage(&[1, 1, 2], 4), vec![25, 25, 50]);
    // 2/2/6 (ten voters): 20/20/60, no gap.
    assert_eq!(get_vote_percentage(&[2, 2, 6], 10), vec![20, 20, 60]);
    // 1/2/4 (seven voters): 14/28/57 sums 99; the 2-vote option has the
    // only gap ≤ total/2, so its whole equal-count group takes the point.
    assert_eq!(get_vote_percentage(&[1, 2, 4], 7), vec![14, 29, 57]);
}

#[test]
fn vote_percentage_halves_round_to_fifty() {
    // The half rule: on an even total, an option sitting on gap == total/2
    // with ≥ 50% never rounds up (PollManager.cpp:686-689); smaller groups
    // do.
    // 1/2/4 (seven voters): the single top-up lands on the 2-vote group.
    assert_eq!(get_vote_percentage(&[1, 2, 4], 7), vec![14, 29, 57]);
    // 1/1 (three voters): the total clamps to the sum 2 → 50/50.
    assert_eq!(get_vote_percentage(&[1, 1], 3), vec![50, 50]);
    // 0/1/1 (two voters): exact 0/50/50 — the halves keep 50.
    assert_eq!(get_vote_percentage(&[0, 1, 1], 2), vec![0, 50, 50]);
    // 1/1/1/2 (five voters): exact 20/20/20/40.
    assert_eq!(get_vote_percentage(&[1, 1, 1, 2], 5), vec![20, 20, 20, 40]);
    // 3/3/2 (eight voters): 37/37/25 = 99; both 3-groups have gap == 4 ==
    // total/2 but 37 < 50, the count 2 exceeds the leftover 1, so nothing
    // moves.
    assert_eq!(get_vote_percentage(&[3, 3, 2], 8), vec![37, 37, 25]);
    // 1/1/1 (three voters): every gap is 2 > total/2 — no wrong-direction
    // rounding, the sum stays 99.
    assert_eq!(get_vote_percentage(&[1, 1, 1], 3), vec![33, 33, 33]);
    // 5/2/1 (eight voters): the 62% option sits on gap == total/2 — the
    // half rule keeps it there while the 12% option takes the point.
    assert_eq!(get_vote_percentage(&[5, 2, 1], 8), vec![62, 25, 13]);
}

// ---------------------------------------------------------------------------
// PollOption store/parse
// ---------------------------------------------------------------------------

fn sample_poll_option() -> PollOption {
    PollOption {
        text: FormattedText {
            text: "Option \u{1F389}".to_owned(),
            entities: vec![italic_entity(0, 6)],
        },
        media_type: None,
        added_by_dialog_id: DialogId::from_user_id(UserId::new(123)),
        recent_voter_dialog_ids: vec![
            DialogId::from_user_id(UserId::new(1)),
            DialogId::from_user_id(UserId::new(2)),
        ],
        added_date: 1700000000,
        voter_count: 7,
        is_chosen: true,
        data: "vote-payload-bytes".to_owned(),
    }
}

#[test]
fn poll_option_store_parse_round_trips_every_field() {
    let option = sample_poll_option();
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    option.store(&mut env, &mut out);

    let mut parser = Parser::new(&out);
    let parsed = PollOption::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();

    assert_eq!(parsed.text, option.text);
    assert_eq!(parsed.data, option.data);
    assert_eq!(parsed.added_by_dialog_id, option.added_by_dialog_id);
    assert_eq!(parsed.added_date, option.added_date);
    assert_eq!(
        parsed.recent_voter_dialog_ids,
        option.recent_voter_dialog_ids
    );
    assert_eq!(parsed.voter_count, option.voter_count);
    assert!(parsed.is_chosen);
    assert_eq!(parsed.media_type, None);
}

#[test]
fn poll_option_store_omits_default_optionals() {
    let option = PollOption {
        text: FormattedText {
            text: "Plain".to_owned(),
            entities: Vec::new(),
        },
        ..PollOption::default()
    };
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    option.store(&mut env, &mut out);

    // flags block + "Plain" (1 length byte + 5 bytes padded to 8) —
    // nothing else.
    assert_eq!(out.len(), 4 + 8);
    let mut parser = Parser::new(&out);
    let parsed = PollOption::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.text.text, "Plain");
    assert!(!parsed.is_chosen);
    assert_eq!(parsed.voter_count, 0);
    assert_eq!(parsed.data, "");
}

#[test]
fn poll_option_media_flag_rides_the_seam() {
    // The has_media path stores through the seam; the record keeps the
    // content-type tag (the M.9 integration owns the bytes).
    let mut option = sample_poll_option();
    option.media_type = Some(9);
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    option.store(&mut env, &mut out);
    assert_eq!(env.stored_media, vec![9], "the seam saw the content tag");

    // parse fails honestly on the seam (the bytes belong to the M.9 codec)
    let mut parser = Parser::new(&out);
    assert!(PollOption::parse(&mut env, &mut parser).is_err());
}

#[test]
fn poll_option_equality_ignores_voter_machine_fields() {
    // PollOption.cpp:162-173 — voter_count/recent voters/is_chosen don't
    // compare; the media compares by type tag only.
    let lhs = sample_poll_option();
    let mut rhs = sample_poll_option();
    rhs.voter_count += 1;
    rhs.is_chosen = !rhs.is_chosen;
    rhs.recent_voter_dialog_ids.clear();
    assert!(PollOption::options_equal(&lhs, &rhs));

    let mut with_media = sample_poll_option();
    with_media.media_type = Some(3);
    assert!(!PollOption::options_equal(&lhs, &with_media));

    let mut different_text = sample_poll_option();
    different_text.text.text = "Other".to_owned();
    assert!(!PollOption::options_equal(&lhs, &different_text));
}

// ---------------------------------------------------------------------------
// Poll store/parse
// ---------------------------------------------------------------------------

fn sample_poll() -> Poll {
    Poll {
        question: FormattedText {
            text: "Question?".to_owned(),
            entities: vec![custom_emoji_entity(0, 8, 12345)],
        },
        options: vec![sample_poll_option(), {
            let mut option = sample_poll_option();
            option.is_chosen = false;
            option.voter_count = 3;
            option.data = "second".to_owned();
            option
        }],
        option_min_channels: vec![(
            ChannelId::new(1001),
            MinChannel {
                title_: "Min chat".to_owned(),
                photo_: crate::photo::DialogPhoto::default(),
                accent_color_id_: AccentColorId::new(7),
                is_megagroup_: true,
            },
        )],
        recent_option_voter_min_channels: Vec::new(),
        recent_voter_dialog_ids: vec![DialogId::from_user_id(UserId::new(42))],
        recent_voter_min_channels: vec![(
            ChannelId::new(1002),
            MinChannel {
                title_: String::new(),
                photo_: crate::photo::DialogPhoto::default(),
                accent_color_id_: AccentColorId::new(0),
                is_megagroup_: false,
            },
        )],
        explanation: FormattedText {
            text: "Because".to_owned(),
            entities: vec![italic_entity(0, 7)],
        },
        explanation_media_type: None,
        correct_option_ids: vec![0, 1],
        total_voter_count: 10,
        open_period: 300,
        close_date: 1700000600,
        hash: 987654321,
        country_codes: vec!["RU".to_owned(), "US".to_owned()],
        is_anonymous: false,
        allow_multiple_answers: true,
        has_open_answers: true,
        has_revoting_disabled: false,
        subscribers_only: true,
        shuffle_answers: true,
        hide_results_until_close: true,
        is_quiz: true,
        is_closed: false,
        is_updated_after_close: false,
        is_creator: true,
        has_unread_votes: true,
        can_view_stats: true,
    }
}

#[test]
fn poll_store_parse_round_trips_every_field() {
    let poll = sample_poll();
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    poll.store(&mut env, &mut out);

    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();

    assert_eq!(parsed.question, poll.question);
    assert_eq!(parsed.options, poll.options);
    assert_eq!(parsed.total_voter_count, 10);
    assert_eq!(parsed.correct_option_ids, vec![0, 1]);
    assert_eq!(parsed.open_period, 300);
    assert_eq!(parsed.close_date, 1700000600);
    assert_eq!(parsed.explanation, poll.explanation);
    assert_eq!(parsed.recent_voter_dialog_ids, poll.recent_voter_dialog_ids);
    assert_eq!(
        parsed.recent_voter_min_channels,
        poll.recent_voter_min_channels
    );
    assert_eq!(parsed.option_min_channels, poll.option_min_channels);
    assert_eq!(parsed.hash, 987654321);
    assert_eq!(parsed.country_codes, poll.country_codes);
    assert!(!parsed.is_anonymous);
    assert!(parsed.allow_multiple_answers);
    assert!(parsed.has_open_answers);
    assert!(!parsed.has_revoting_disabled);
    assert!(parsed.subscribers_only);
    assert!(parsed.shuffle_answers);
    assert!(parsed.hide_results_until_close);
    assert!(parsed.is_quiz);
    assert!(!parsed.is_closed);
    assert!(!parsed.is_updated_after_close);
    assert!(parsed.is_creator);
    assert!(parsed.has_unread_votes);
    assert!(parsed.can_view_stats);
}

#[test]
fn poll_store_parse_regular_poll_minimal_form() {
    // The non-quiz minimal record: the single correct-option form and the
    // optional blocks are all absent.
    let poll = Poll {
        question: FormattedText {
            text: "Yes/No?".to_owned(),
            entities: Vec::new(),
        },
        options: vec![PollOption {
            text: FormattedText {
                text: "Yes".to_owned(),
                entities: Vec::new(),
            },
            data: "opt1".to_owned(),
            voter_count: 0,
            ..PollOption::default()
        }],
        is_anonymous: true,
        ..Poll::default()
    };
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    poll.store(&mut env, &mut out);

    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.question.text, "Yes/No?");
    assert_eq!(parsed.options.len(), 1);
    assert_eq!(parsed.options[0].data, "opt1");
    assert!(parsed.is_anonymous);
    assert!(!parsed.is_quiz);
    assert_eq!(parsed.correct_option_ids, Vec::<i32>::new());
}

#[test]
fn poll_store_parse_quiz_single_correct_option() {
    // The non-multiple correct ids are stored as the single int form
    // (-1 encodes "empty").
    let mut poll = sample_poll();
    poll.correct_option_ids = vec![1];
    poll.option_min_channels.clear();
    poll.recent_voter_min_channels.clear();
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    poll.store(&mut env, &mut out);

    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.correct_option_ids, vec![1]);

    // The -1 encoding parses as an empty list.
    poll.correct_option_ids.clear();
    let mut out = Vec::new();
    poll.store(&mut env, &mut out);
    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.correct_option_ids, Vec::<i32>::new());

    // An out-of-range correct id corrupts the record (the C++ sets the
    // parser error; the Rust band is the generic corruption error).
    let mut out = Vec::new();
    store_flags(
        &mut out,
        [
            false, false, false, true, false, false, false, false, false, false, false, false,
            false, false, true, false, false, false, false, false, false, false, false, false,
            false, false, false,
        ],
    );
    store_string(&mut out, "Q");
    store_i32(&mut out, 1); // one option
    {
        let option = PollOption {
            text: FormattedText {
                text: "Yes".to_owned(),
                entities: Vec::new(),
            },
            ..PollOption::default()
        };
        option.store(&mut env, &mut out);
    }
    store_i32(&mut out, 0);
    store_i32(&mut out, 5); // the out-of-range correct option id
    let mut parser = Parser::new(&out);
    assert!(Poll::parse(&mut env, &mut parser).is_err());
}

#[test]
fn poll_parse_legacy_recent_voter_user_ids() {
    // The pre-flags-era bit 4 form: recent voters stored as user ids
    // (PollManager.hpp:137, 182-186).
    let mut out = Vec::new();
    store_flags(
        &mut out,
        [
            false, // is_closed
            false, // is_public (anonymous)
            false, // allow_multiple_answers
            false, // is_quiz
            true,  // has_recent_voter_user_ids — the legacy bit 4 form
            false, // has_open_period
            false, // has_close_date
            false, // has_explanation
            false, // is_updated_after_close
            false, // has_recent_voter_dialog_ids
            false, // has_recent_voter_min_channels
            false, // has_question_entities
            false, // has_multiple_correct_option_ids
            false, // has_open_answers
            true,  // know_revoting_disabled
            false, // has_revoting_disabled
            false, // shuffle_answers
            false, // hide_results_until_close
            false, // is_creator
            false, // has_hash
            false, // has_unread_votes
            false, // has_explanation_media
            false, // has_option_min_channels
            false, // has_recent_option_voter_min_channels
            false, // subscribers_only
            false, // has_country_codes
            false, // can_view_stats
        ],
    );
    store_string(&mut out, "Legacy"); // question text
    store_i32(&mut out, 0); // no options
    store_i32(&mut out, 3); // total_voter_count
    store_i32(&mut out, 2); // two legacy recent-voter user ids
    store_i64(&mut out, 77); // UserId (Support64BitIds: store_long)
    store_i64(&mut out, 78);
    let mut env = TestStoreEnv::default();
    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed.question.text, "Legacy");
    assert_eq!(
        parsed.recent_voter_dialog_ids,
        vec![
            DialogId::from_user_id(UserId::new(77)),
            DialogId::from_user_id(UserId::new(78)),
        ]
    );
}

#[test]
fn poll_parse_revoting_disabled_fallback() {
    // The know_revoting_disabled=false band: has_revoting_disabled rides
    // the quiz flag (PollManager.hpp:205-207) — a quiz becomes
    // revoting-disabled, a regular poll doesn't.
    // Quiz: the fallback sets the flag true regardless of the stored bit.
    let mut out = Vec::new();
    store_flags(
        &mut out,
        [
            false, false, false, true, false, false, false, false, false, false, false, false,
            false, false, false, // know_revoting_disabled = false
            false, false, false, false, false, false, false, false, false, false, false, false,
        ],
    );
    store_string(&mut out, "Q");
    store_i32(&mut out, 0);
    store_i32(&mut out, 0);
    store_i32(&mut out, -1); // the quiz single-correct-id empty form
    let mut env = TestStoreEnv::default();
    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert!(parsed.is_quiz);
    assert!(parsed.has_revoting_disabled, "the fallback rides is_quiz");

    // Regular poll with the stored flag true: the fallback clears it.
    let mut out = Vec::new();
    store_flags(
        &mut out,
        [
            false, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, true, // has_revoting_disabled despite the unknown-flags era
            false, false, false, false, false, false, false, false, false, false, false,
        ],
    );
    store_string(&mut out, "Q");
    store_i32(&mut out, 0);
    store_i32(&mut out, 0);
    let mut parser = Parser::new(&out);
    let parsed = Poll::parse(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert!(!parsed.is_quiz);
    assert!(!parsed.has_revoting_disabled);
}

#[test]
fn min_channel_store_parse_round_trips() {
    // The photo block rides the has_photo flag — small_file_id.is_valid()
    // — so the invalid-placeholder photo drops wholesale, the exact
    // framing the C++ produces for a photo-less MinChannel.
    let min_channels = vec![
        (
            ChannelId::new(1001),
            MinChannel {
                title_: "Chat A".to_owned(),
                photo_: crate::photo::DialogPhoto::default(),
                accent_color_id_: AccentColorId::new(3),
                is_megagroup_: true,
            },
        ),
        (ChannelId::new(1002), MinChannel::default()),
    ];
    let mut out = Vec::new();
    let mut env = TestStoreEnv::default();
    store_min_channels(&min_channels, &mut env, &mut out);

    let mut parser = Parser::new(&out);
    let parsed = parse_min_channels(&mut env, &mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed, min_channels);
}

// ---------------------------------------------------------------------------
// The manager log events
// ---------------------------------------------------------------------------

#[test]
fn set_poll_answer_log_event_round_trips() {
    let event = SetPollAnswerLogEvent {
        poll_id: PollId::new(-12345),
        message_full_id: MessageFullId::new(
            DialogId::from_user_id(UserId::new(9)),
            MessageId::new(1 << 20),
        ),
        options: vec!["opt-a".to_owned(), "opt-b".to_owned()],
    };
    let mut out = Vec::new();
    store_log_event_version(&mut out);
    event.store(&mut out);

    let mut parser = Parser::new_log_event(&out).unwrap();
    let parsed = SetPollAnswerLogEvent::parse(&mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn stop_poll_log_event_round_trips() {
    let event = StopPollLogEvent {
        poll_id: PollId::new(777),
        message_full_id: MessageFullId::new(
            DialogId::from_channel_id(ChannelId::new(1003)),
            MessageId::new(2 << 20),
        ),
    };
    let mut out = Vec::new();
    store_log_event_version(&mut out);
    event.store(&mut out);

    let mut parser = Parser::new_log_event(&out).unwrap();
    let parsed = StopPollLogEvent::parse(&mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn status_error_bands_match_cpp_strings() {
    // the Status::Error(400, ...) texts the td_api layer surfaces
    let error = check_quiz_correct_option_ids(&[], 3, false).unwrap_err();
    assert_eq!(error.code(), 400);
    assert_eq!(
        error.message(),
        "Correct quiz option list must be non-empty"
    );
    let _: Result<(), Status> = check_quiz_correct_option_ids(&[0], 1, false);
}
