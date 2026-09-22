//! Port of `td/telegram`: client logic, managers, td_api query/update
//! handling. The td_api data model is generated at build time from the
//! pinned scheme (S09, store in S10).
//!
//! Track H: S67 adds the C ABI crate (`tdjson`), S68 the JSON client
//! plumbing ([`client_json`]) and the synchronous request set
//! ([`synchronous_requests`]); the full ClientManager port is S70, the
//! Options manager S69.

pub mod auth_manager;
pub mod birthdate;
pub mod chat_manager;
pub mod client;
pub mod client_json;
pub mod config_dispatch;
pub mod config_manager;
pub mod config_recoverer;
pub mod config_result;
pub mod connection_creator;
pub mod contact;
pub mod convert;
pub mod device_token_manager;
pub mod dialog_db;
pub mod dialog_id;
pub mod dialog_manager;
pub mod document;
pub mod emoji_status;
pub mod files;
pub mod formatted_date;
pub mod global;
pub mod link_manager;
pub mod manager_option_states;
pub mod message_db;
pub mod message_entity;
pub mod message_id;
pub mod misc;
pub mod net;
pub mod notification;
pub mod notification_db;
pub mod notification_manager;
pub mod notification_settings;
pub mod notification_settings_manager;
pub mod option_manager;
pub mod peer_color;
pub mod photo;
pub mod query_combiner;
pub mod query_merger;
pub mod reaction_type;
pub mod request_actor;
pub mod requests;
pub mod restriction_reason;
pub mod store_parse;
pub mod story_album;
pub mod story_content;
pub mod story_db;
pub mod story_forward_info;
pub mod story_ids;
pub mod story_interaction_info;
pub mod story_manager;
pub mod story_notification_settings;
pub mod story_stealth_mode;
pub mod story_viewer;
pub mod synchronous_requests;
pub mod td;
pub mod time_zone_manager;
pub mod updates_manager;
pub mod user_manager;
pub mod usernames;

// Generated code keeps C++-identical case-preserved names (`aiComposeTone`)
// in types, fields and the JSON dispatch helpers alike.
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::large_enum_variant
)]
pub mod td_api {
    include!(concat!(env!("OUT_DIR"), "/td_api.rs"));
}

#[cfg(test)]
mod tests {
    use super::td_api;

    #[test]
    fn generated_model_counts_match_the_scheme() {
        // Fixed by the S07 differential dump against td's own tl-parser:
        // 753 types / 2192 ctors / 1022 functions. Every non-builtin ctor
        // (2192 - 9) and every function get a struct; multi-ctor types (211)
        // additionally get an enum, plus the Function umbrella.
        assert_eq!(td_api::SCHEME_TYPES, 753);
        assert_eq!(td_api::SCHEME_CTORS, 2192);
        assert_eq!(td_api::SCHEME_FUNCTIONS, 1022);
        assert_eq!(td_api::GENERATED_STRUCTS, 3205);
        assert_eq!(td_api::GENERATED_ENUMS, 211);
    }

    #[test]
    fn model_is_constructible_and_debug_prints() {
        let content =
            td_api::MessageContent::messageChatDeletePhoto(td_api::messageChatDeletePhoto);
        assert!(format!("{content:?}").contains("messageChatDeletePhoto"));
        let text = td_api::FormattedText {
            text: "hi".to_owned(),
            entities: Vec::new(),
        };
        assert_eq!(text.text, "hi");
        let _ok: td_api::Ok = td_api::Ok;
    }

    #[test]
    fn internally_tagged_enum_round_trips() {
        let content =
            td_api::MessageContent::messageChatDeletePhoto(td_api::messageChatDeletePhoto);
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, r#"{"@type":"messageChatDeletePhoto"}"#);
        let back: td_api::MessageContent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, content);
    }

    #[test]
    fn struct_round_trips_with_verbatim_field_names() {
        let text = td_api::FormattedText {
            text: "hi".to_owned(),
            entities: Vec::new(),
        };
        let json = serde_json::to_string(&text).unwrap();
        assert_eq!(json, r#"{"text":"hi","entities":[]}"#);
        let back: td_api::FormattedText = serde_json::from_str(&json).unwrap();
        assert_eq!(back, text);
    }

    #[test]
    fn object_umbrella_dynamic_round_trip() {
        // S16: the dynamic surface — td's to_json(const Object&) switch and
        // tl_constructor_from_string(Object), by name and by numeric id.
        let value = td_api::Object::error(td_api::Error {
            code: 400,
            message: "Bad Request: test".to_owned(),
        });
        let json = value.to_json();
        assert_eq!(
            json,
            r#"{"@type":"error","code":400,"message":"Bad Request: test"}"#
        );
        assert_eq!(td_api::Object::from_json(&json).unwrap(), value);
        let numeric = format!(
            r#"{{"@type":{},"code":404,"message":"nf"}}"#,
            td_api::Error::ID
        );
        assert_eq!(
            td_api::Object::from_json(&numeric).unwrap(),
            td_api::Object::error(td_api::Error {
                code: 404,
                message: "nf".to_owned()
            })
        );
        assert_eq!(
            td_api::Object::from_json(r#"{"@type":"nope"}"#).unwrap_err(),
            "Unknown class \"nope\""
        );
    }
}
