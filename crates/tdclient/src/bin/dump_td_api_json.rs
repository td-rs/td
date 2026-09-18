//! S12/S16 verification: builds the same td_api samples as the C++ oracle
//! (`conformance/tl_json_oracle/dump_json.cpp`) from the tl-codegen-
//! generated model, prints their `to_json` plus the Object-umbrella
//! (`to_json(const Object&)`) form of each, then parses the same request
//! JSONs through `Function::from_json` (accept/reject + error message —
//! td_api has no C++ binary store to diff against). verify.sh diffs the
//! two outputs byte for byte.
//!
//! Nullable-object parity note: td's writer omits null object fields; the
//! Rust model materializes defaults, so every sample fills its object
//! fields — same convention as the C++ oracle.

use tdclient::td_api;

fn dump(name: &str, json: &str) {
    println!("{name}:{json}");
}

/// S16: the sample's own to_json plus the same value through the Object
/// umbrella — the dynamic response-side surface (td's generated
/// `to_json(const Object&)` switch).
fn dump2(name: &str, json: &str, object: td_api::Object) {
    dump(name, json);
    println!("{name}:obj:{}", object.to_json());
}

fn dump_parse(name: &str, json: &str) {
    match td_api::Function::from_json(json) {
        Ok(_) => println!("{name}:ok"),
        Err(e) => println!("{name}:err:{e}"),
    }
}

fn main() {
    // ---- to_json samples --------------------------------------------------
    dump2("ok", &td_api::Ok.to_json(), td_api::Object::ok(td_api::Ok));

    {
        let obj = td_api::Error {
            code: -42,
            message: "line1\n\"quoted\" \\ \u{1} é中😀 tab\tend".to_owned(),
        };
        dump2("error", &obj.to_json(), td_api::Object::error(obj));
    }

    {
        let obj = td_api::Minithumbnail {
            width: 640,
            height: 360,
            data: vec![0x00, 0x01, 0xff, b' ', b't', b'l'],
        };
        dump2(
            "minithumbnail",
            &obj.to_json(),
            td_api::Object::minithumbnail(obj),
        );
    }
    dump2(
        "minithumbnail_empty",
        &td_api::Minithumbnail::default().to_json(),
        td_api::Object::minithumbnail(td_api::Minithumbnail::default()),
    );

    {
        let obj = td_api::FormattedText {
            text: "héllo 中 😀 <>&".to_owned(),
            entities: vec![td_api::TextEntity {
                offset: 2,
                length: 3,
                r#type: Some(td_api::TextEntityType::textEntityTypeBold(
                    td_api::textEntityTypeBold,
                )),
            }],
        };
        dump2(
            "formattedText",
            &obj.to_json(),
            td_api::Object::formattedText(obj),
        );
    }

    {
        let obj = td_api::File {
            id: 7,
            size: 1234567890123,
            expected_size: 9876543210987,
            local: Some(td_api::LocalFile {
                path: "/tmp/x.bin".to_owned(),
                is_downloading_active: true,
                downloaded_size: 4096,
                ..Default::default()
            }),
            remote: Some(td_api::RemoteFile {
                unique_id: "AQID".to_owned(),
                ..Default::default()
            }),
        };
        dump2("file", &obj.to_json(), td_api::Object::file(obj));
    }

    {
        let obj = td_api::MessageForwardInfo {
            origin: Some(td_api::MessageOrigin::messageOriginUser(
                td_api::messageOriginUser { sender_user_id: 42 },
            )),
            date: 1700000123,
            source: Some(td_api::ForwardSource {
                chat_id: -1001234567890,
                message_id: 555,
                sender_id: Some(td_api::MessageSender::messageSenderChat(
                    td_api::messageSenderChat { chat_id: -99 },
                )),
                sender_name: "fwd".to_owned(),
                date: 7,
                is_outgoing: true,
            }),
            public_service_announcement_type: "psa".to_owned(),
        };
        dump2(
            "messageForwardInfo",
            &obj.to_json(),
            td_api::Object::messageForwardInfo(obj),
        );
    }

    {
        let obj = td_api::EmojiStatusCustomEmojis {
            custom_emoji_ids: vec![0, -1, 0x1234_5678_9abc_def0, i64::MAX],
        };
        dump2(
            "emojiStatusCustomEmojis",
            &obj.to_json(),
            td_api::Object::emojiStatusCustomEmojis(obj),
        );
    }

    {
        let obj = td_api::Location {
            latitude: 1.5,
            longitude: -2.25,
            horizontal_accuracy: 0.0,
        };
        dump2("location", &obj.to_json(), td_api::Object::location(obj));
    }
    {
        let obj = td_api::Location {
            latitude: 1e-7,
            longitude: -0.0,
            horizontal_accuracy: 123456.78125,
        };
        dump2(
            "location_edges",
            &obj.to_json(),
            td_api::Object::location(obj),
        );
    }

    {
        let obj = td_api::Poll {
            id: -9007199254740993,
            question: Some(td_api::FormattedText {
                text: "q?".to_owned(),
                entities: Vec::new(),
            }),
            options: Vec::new(),
            total_voter_count: 4,
            recent_voter_ids: vec![td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser { user_id: 0 },
            )],
            can_get_voters: true,
            can_see_results: false,
            is_anonymous: true,
            allows_multiple_answers: false,
            allows_revoting: false,
            members_only: true,
            country_codes: vec!["EN".to_owned(), String::new()],
            option_order: vec![0],
            r#type: Some(td_api::PollType::pollTypeRegular(td_api::pollTypeRegular)),
            open_period: 300,
            close_date: 0,
            is_closed: false,
            vote_restriction_reason: Some(
                td_api::PollVoteRestrictionReason::pollVoteRestrictionReasonClosed(
                    td_api::pollVoteRestrictionReasonClosed,
                ),
            ),
        };
        dump2("poll", &obj.to_json(), td_api::Object::poll(obj));
    }

    // ---- from_json samples (parse → binary store / error) ------------------
    dump_parse(
        "fn_getMessage",
        "{\"@type\":\"getMessage\",\"chat_id\":-100200300400500,\"message_id\":600700800900}",
    );
    dump_parse(
        "fn_ints_as_strings",
        "{\"@type\":\"getMessage\",\"chat_id\":\"123\",\"message_id\":\"456\"}",
    );
    dump_parse("fn_missing_fields", "{\"@type\":\"getMessage\"}");
    dump_parse(
        "fn_null_and_extra",
        "{\"@type\":\"getMessage\",\"chat_id\":null,\"message_id\":9,\"@extra\":\"ignored\"}",
    );
    dump_parse(
        "fn_bool_from_int",
        "{\"@type\":\"toggleChatIsMarkedAsUnread\",\"chat_id\":1,\"is_marked_as_unread\":1}",
    );
    dump_parse(
        "fn_vec_string",
        "{\"@type\":\"testCallVectorString\",\"x\":[\"\",\"a\",\"multi\\nline\"]}",
    );
    dump_parse(
        "fn_bytes",
        "{\"@type\":\"testCallBytes\",\"x\":\"dGwAAQ==\"}",
    );
    dump_parse(
        "fn_numeric_type",
        &format!(
            "{{\"@type\":{},\"chat_id\":1,\"message_id\":2}}",
            td_api::getMessage::ID
        ),
    );
    dump_parse("err_unknown_ctor", "{\"@type\":\"nonexistentFn\"}");
    dump_parse("err_bad_json", "garbage");
    dump_parse("err_trailing", "{\"@type\":\"getMessage\"} x");
    dump_parse("err_frac_int", "{\"@type\":\"getMessage\",\"chat_id\":1.5}");
    dump_parse(
        "err_int_junk",
        "{\"@type\":\"getMessage\",\"chat_id\":\"12x\"}",
    );
    dump_parse(
        "err_int53_overflow",
        "{\"@type\":\"getMessage\",\"chat_id\":9223372036854775808}",
    );
    dump_parse(
        "err_wrong_type",
        "{\"@type\":\"getMessage\",\"chat_id\":\"str\"}",
    );
}
