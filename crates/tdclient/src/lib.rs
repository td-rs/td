//! Port of `td/telegram`: client logic, managers, td_api query/update
//! handling. The td_api data model is generated at build time from the
//! pinned scheme (S09).

#[allow(dead_code, clippy::large_enum_variant)]
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
            td_api::MessageContent::MessageChatDeletePhoto(td_api::MessageChatDeletePhoto);
        assert!(format!("{content:?}").contains("MessageChatDeletePhoto"));
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
            td_api::MessageContent::MessageChatDeletePhoto(td_api::MessageChatDeletePhoto);
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
}
