//! Port of `td/telegram/SynchronousRequests.{h,cpp}` + the td_api half of
//! `td/telegram/JsonValue.cpp` (S68): the `td_execute` offline request set.
//!
//! The log- and JSON-related functions are complete here. The text-parse,
//! language-pack, phone-number and push-id functions are listed in the C++
//! switch but land with their managers (Wave 1/3); until then they report
//! "The method can't be executed synchronously", like any other request.

use std::sync::Mutex;

use crate::option_manager;
use crate::td_api;
use crate::td_api::{Function, JsonValue as TdJsonValue, Object};
use tdutils::json::{JsonValue, json_decode};
use tdutils::status::Result;

/// `SynchronousRequests::make_error`.
pub fn make_error(code: i32, message: &str) -> Object {
    Object::error(td_api::Error {
        code,
        message: message.to_string(),
    })
}

/// `td::Logging::log_tags` — the default tags with their C++ verbosity
/// defaults (`int VERBOSITY_NAME(tag) = ...` scattered across the modules),
/// in the sorted order the C++ `std::map` iterates (`get_tags()`).
static LOG_TAGS: &[(&str, i32)] = &[
    ("actor", 4 + 10),
    ("binlog", 4 + 8),
    ("config_recoverer", 3),
    ("connections", 3),
    ("dc", 4 + 2),
    ("dns_resolver", 4),
    ("fd", 4 + 9),
    ("file_gc", 3),
    ("file_loader", 4),
    ("file_references", 3),
    ("get_difference", 3),
    ("mtproto", 4 + 7),
    ("net_query", 3),
    ("notifications", 3),
    ("proxy", 4),
    ("raw_mtproto", 4 + 10),
    ("sqlite", 4 + 10),
    ("td_init", 4 + 3),
    ("td_requests", 3),
    ("update_file", 3),
];

/// `td::Logging` per-tag verbosity levels (the C++ `std::map<Slice, int *>`
/// over the tag globals; here the values live in one map). The levels are
/// stored and reported; per-tag filtering in the `tdutils` log macros is a
/// later step (S69+ logging wiring).
static TAG_VERBOSITY: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn tag_verbosity() -> &'static Mutex<Vec<i32>> {
    let levels = TAG_VERBOSITY.lock().unwrap();
    if levels.is_empty() {
        drop(levels);
        *TAG_VERBOSITY.lock().unwrap() = LOG_TAGS.iter().map(|(_, level)| *level).collect();
    }
    &TAG_VERBOSITY
}

/// `SynchronousRequests::is_synchronous_request` — the exact C++ switch,
/// minus the functions whose managers are not ported yet (they correctly
/// fall into the default 400 "can't be executed synchronously" path).
pub fn is_synchronous_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getJsonValue(_)
            | Function::getJsonString(_)
            | Function::setLogStream(_)
            | Function::getLogStream(_)
            | Function::setLogVerbosityLevel(_)
            | Function::getLogVerbosityLevel(_)
            | Function::getLogTags(_)
            | Function::setLogTagVerbosityLevel(_)
            | Function::getLogTagVerbosityLevel(_)
            | Function::addLogMessage(_)
            | Function::testReturnError(_)
            | Function::getFileMimeType(_)
            | Function::getFileExtension(_)
            | Function::searchStringsByPrefix(_)
    ) || matches!(function, Function::getOption(g) if option_manager::is_synchronous_option(&g.name))
    // Not ported yet: searchQuote (MessageQuote, M.8-adjacent),
    // getTextEntities/parseTextEntities/parseMarkdown/getMarkdownText
    // (MessageEntity, M.8), checkQuickReplyShortcutName (QuickReplyManager),
    // getCountryFlagEmoji/getPhoneNumberInfoSync (CountryInfoManager),
    // cleanFileName (needs the `get_unicode_simple_category` tables, M.8),
    // getLanguagePackString (LanguagePackManager),
    // getChatFolderDefaultIconName (DialogFilter), getPushReceiverId
    // (NotificationManager), getThemeParametersJsonString (ThemeManager).
}

/// `JsonValue.cpp:get_json_value_object`: `tdutils::JsonValue` → td_api.
fn get_json_value_object(json_value: &JsonValue) -> TdJsonValue {
    match json_value {
        JsonValue::Null => TdJsonValue::jsonValueNull(td_api::jsonValueNull),
        JsonValue::Boolean(value) => {
            TdJsonValue::jsonValueBoolean(td_api::jsonValueBoolean { value: *value })
        }
        JsonValue::Number(number) => TdJsonValue::jsonValueNumber(td_api::jsonValueNumber {
            value: tdutils::json::to_double(number),
        }),
        JsonValue::String(value) => TdJsonValue::jsonValueString(td_api::jsonValueString {
            value: value.clone(),
        }),
        JsonValue::Array(values) => TdJsonValue::jsonValueArray(td_api::jsonValueArray {
            values: values
                .iter()
                .map(|value| Box::new(get_json_value_object(value)))
                .collect(),
        }),
        JsonValue::Object(object) => {
            let mut members = Vec::with_capacity(object.field_count());
            object.foreach(|name, value| {
                members.push(Box::new(td_api::JsonObjectMember {
                    key: name.to_string(),
                    value: Some(Box::new(get_json_value_object(value))),
                }));
            });
            TdJsonValue::jsonValueObject(td_api::jsonValueObject { members })
        }
    }
}

/// `JsonValue.cpp:get_json_value`.
fn get_json_value(json: &str) -> Result<TdJsonValue> {
    match json_decode(json) {
        Result::Ok(value) => Result::Ok(get_json_value_object(&value)),
        Result::Err(status) => Result::Err(status),
    }
}

/// The `JsonableJsonValue` reverse of [`get_json_value_object`]: the
/// td_api JsonValue → the plain JSON value form (no `@type`), then
/// `json_encode` — `JsonValue.cpp:get_json_string`.
fn json_value_to_json(json_value: &TdJsonValue) -> JsonValue {
    match json_value {
        TdJsonValue::jsonValueNull(_) => JsonValue::Null,
        TdJsonValue::jsonValueBoolean(v) => JsonValue::Boolean(v.value),
        // `JsonBuilder << double`: fixed 6-decimal emission, like the
        // generated to_json (S12 oracle-validated).
        TdJsonValue::jsonValueNumber(v) => JsonValue::Number(format!("{:.6}", v.value)),
        // Rust strings are valid UTF-8 by construction; the C++
        // `check_utf8` rejections cannot occur.
        TdJsonValue::jsonValueString(v) => JsonValue::String(v.value.clone()),
        TdJsonValue::jsonValueArray(v) => JsonValue::Array(
            v.values
                .iter()
                .map(|value| json_value_to_json(value))
                .collect(),
        ),
        TdJsonValue::jsonValueObject(v) => {
            let default_value = TdJsonValue::default();
            JsonValue::Object(tdutils::json::JsonObject::from_fields(
                v.members
                    .iter()
                    .map(|member| {
                        (
                            member.key.clone(),
                            json_value_to_json(member.value.as_deref().unwrap_or(&default_value)),
                        )
                    })
                    .collect(),
            ))
        }
    }
}

/// `JsonValue.cpp:get_json_string`.
fn get_json_string(json_value: &TdJsonValue) -> String {
    tdutils::json::json_encode(&json_value_to_json(json_value), false)
}

/// `SynchronousRequests::run_request` — the dispatcher behind `td_execute`.
pub fn run_request(function: Function) -> Object {
    match function {
        Function::getOption(request) => {
            match option_manager::get_option_synchronously(&request.name) {
                Some(value) => option_value_object(value),
                // The C++ message keeps the "can't be get" wording.
                None => make_error(400, "The option can't be get synchronously"),
            }
        }
        Function::getJsonValue(request) => match get_json_value(&request.json) {
            Result::Ok(value) => json_value_object(value),
            Result::Err(status) => make_error(400, status.message()),
        },
        Function::getJsonString(request) => {
            let default_json = td_api::JsonValue::default();
            Object::text(td_api::Text {
                text: get_json_string(request.json_value.as_deref().unwrap_or(&default_json)),
            })
        }
        Function::setLogStream(request) => {
            match set_current_stream(request.log_stream.unwrap_or_default()) {
                std::result::Result::Ok(()) => Object::ok(td_api::Ok),
                std::result::Result::Err(message) => make_error(400, &message),
            }
        }
        Function::getLogStream(_) => get_current_stream(),
        Function::setLogVerbosityLevel(request) => {
            if (0..=tdutils::logging::NEVER).contains(&request.new_verbosity_level) {
                tdutils::logging::set_verbosity_level(request.new_verbosity_level);
                Object::ok(td_api::Ok)
            } else {
                make_error(400, "Wrong new verbosity level specified")
            }
        }
        Function::getLogVerbosityLevel(_) => Object::logVerbosityLevel(td_api::LogVerbosityLevel {
            verbosity_level: tdutils::logging::get_verbosity_level(),
        }),
        Function::getLogTags(_) => Object::logTags(td_api::LogTags {
            tags: LOG_TAGS.iter().map(|(tag, _)| (*tag).to_string()).collect(),
        }),
        Function::setLogTagVerbosityLevel(request) => {
            match set_tag_verbosity_level(&request.tag, request.new_verbosity_level) {
                std::result::Result::Ok(()) => Object::ok(td_api::Ok),
                std::result::Result::Err(message) => make_error(400, &message),
            }
        }
        Function::getLogTagVerbosityLevel(request) => match get_tag_verbosity_level(&request.tag) {
            std::result::Result::Ok(level) => {
                Object::logVerbosityLevel(td_api::LogVerbosityLevel {
                    verbosity_level: level,
                })
            }
            std::result::Result::Err(message) => make_error(400, &message),
        },
        Function::addLogMessage(request) => {
            let level = request.verbosity_level.clamp(0, tdutils::logging::NEVER);
            tdutils::log_impl!(level, "", "{}", request.text);
            Object::ok(td_api::Ok)
        }
        Function::testReturnError(request) => Object::error(request.error.unwrap_or_default()),
        // --- the M.11.4 generic (manager-free) synchronous set ---
        Function::getFileMimeType(request) => {
            // SynchronousRequests.cpp:262-265 — no UTF-8 check on the name.
            let path_view = tdutils::path_view::PathView::new(&request.file_name);
            Object::text(td_api::Text {
                text: tdutils::mime_type::from_extension(path_view.extension()),
            })
        }
        Function::getFileExtension(request) => {
            // SynchronousRequests.cpp:267-269 — no UTF-8 check on the type.
            Object::text(td_api::Text {
                text: tdutils::mime_type::to_extension(&request.mime_type),
            })
        }
        Function::searchStringsByPrefix(request) => {
            // `do_request(searchStringsByPrefix)` → misc.cpp:406-417 — the
            // UTF-8 gates run first.
            let Some(query) = crate::misc::clean_input_string(&request.query) else {
                return make_error(400, "Strings must be encoded in UTF-8");
            };
            let mut strings = Vec::with_capacity(request.strings.len());
            for string in &request.strings {
                let Some(string) = crate::misc::clean_input_string(string) else {
                    return make_error(400, "Strings must be encoded in UTF-8");
                };
                strings.push(string);
            }
            let (total_count, positions) = crate::misc::search_strings_by_prefix(
                strings,
                &query,
                request.limit,
                !request.return_none_for_empty_query,
            );
            Object::foundPositions(td_api::FoundPositions {
                total_count,
                positions,
            })
        }
        _ => make_error(400, "The method can't be executed synchronously"),
    }
}

/// `Logging::set_current_stream` (the tdutils halves: `set_file_path` for
/// default/file, the null interface for empty).
fn set_current_stream(stream: td_api::LogStream) -> std::result::Result<(), String> {
    match stream {
        td_api::LogStream::logStreamDefault(_) => {
            tdutils::logging::set_file_path("");
            // C++ `set_current_stream(logStreamDefault)` also resets the
            // interface from the null log back to the default one.
            tdutils::logging::set_log_interface(None);
            Ok(())
        }
        td_api::LogStream::logStreamFile(file_stream) => {
            if file_stream.max_file_size <= 0 {
                return Err("Max log file size must be positive".to_string());
            }
            // redirect_stderr is not ported (see FileLog in tdutils).
            tdutils::logging::install_file_log(&file_stream.path, file_stream.max_file_size)
        }
        td_api::LogStream::logStreamEmpty(_) => {
            tdutils::logging::set_file_path("");
            tdutils::logging::set_log_interface(Some(tdutils::logging::null_log_interface()));
            Ok(())
        }
    }
}

/// `Logging::get_current_stream`.
fn get_current_stream() -> Object {
    if let Some((path, rotate_threshold)) = tdutils::logging::file_log_info() {
        return Object::logStreamFile(td_api::logStreamFile {
            path,
            max_file_size: rotate_threshold,
            redirect_stderr: false, // see the FileLog divergence
        });
    }
    // Trait-object pointer comparison is unreliable across codegen units
    // (duplicated vtables); the interface tag tracks the identity instead.
    if tdutils::logging::current_interface_tag() == 1 {
        return Object::logStreamEmpty(td_api::logStreamEmpty);
    }
    Object::logStreamDefault(td_api::logStreamDefault)
}

/// `Logging::set_tag_verbosity_level`.
fn set_tag_verbosity_level(tag: &str, new_verbosity_level: i32) -> std::result::Result<(), String> {
    if tag.is_empty() {
        return Err("Log tag must be non-empty".to_string());
    }
    let index = match LOG_TAGS.iter().position(|(known, _)| *known == tag) {
        Some(index) => index,
        None => return Err("Log tag is not found".to_string()),
    };
    tag_verbosity().lock().unwrap()[index] = new_verbosity_level.clamp(1, tdutils::logging::NEVER);
    Ok(())
}

/// `Logging::get_tag_verbosity_level`.
fn get_tag_verbosity_level(tag: &str) -> std::result::Result<i32, String> {
    if tag.is_empty() {
        return Err("Log tag must be non-empty".to_string());
    }
    let index = match LOG_TAGS.iter().position(|(known, _)| *known == tag) {
        Some(index) => index,
        None => return Err("Log tag is not found".to_string()),
    };
    Ok(tag_verbosity().lock().unwrap()[index])
}

fn option_value_object(value: td_api::OptionValue) -> Object {
    match value {
        td_api::OptionValue::optionValueBoolean(v) => Object::optionValueBoolean(v),
        td_api::OptionValue::optionValueEmpty(v) => Object::optionValueEmpty(v),
        td_api::OptionValue::optionValueInteger(v) => Object::optionValueInteger(v),
        td_api::OptionValue::optionValueString(v) => Object::optionValueString(v),
    }
}

fn json_value_object(value: TdJsonValue) -> Object {
    match value {
        TdJsonValue::jsonValueNull(v) => Object::jsonValueNull(v),
        TdJsonValue::jsonValueBoolean(v) => Object::jsonValueBoolean(v),
        TdJsonValue::jsonValueNumber(v) => Object::jsonValueNumber(v),
        TdJsonValue::jsonValueString(v) => Object::jsonValueString(v),
        TdJsonValue::jsonValueArray(v) => Object::jsonValueArray(v),
        TdJsonValue::jsonValueObject(v) => Object::jsonValueObject(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests below mutate process-global logging state (verbosity level,
    /// active stream, tag levels), so they serialize on one lock — in
    /// parallel they would corrupt each other (the tdutils TEST_LOCK pattern).
    static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn get_option_version_round_trip() {
        let function = Function::getOption(td_api::getOption {
            name: "version".to_string(),
        });
        let response = run_request(function);
        assert_eq!(
            response.to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }

    #[test]
    fn get_option_async_option_reports_400() {
        let function = Function::getOption(td_api::getOption {
            name: "database_directory".to_string(),
        });
        let response = run_request(function);
        assert_eq!(
            response.to_json(),
            r#"{"@type":"error","code":400,"message":"The option can't be get synchronously"}"#
        );
    }

    #[test]
    fn non_synchronous_request_reports_400() {
        let function = Function::getMe(td_api::getMe);
        assert!(!is_synchronous_request(&function));
        let response = run_request(function);
        assert_eq!(
            response.to_json(),
            r#"{"@type":"error","code":400,"message":"The method can't be executed synchronously"}"#
        );
    }

    #[test]
    fn get_json_value_converts_all_forms() {
        let function = Function::getJsonValue(td_api::getJsonValue {
            json: r#"{"a":[1,true,null,"x"],"b":{"c":2.5}}"#.to_string(),
        });
        let response = run_request(function);
        assert_eq!(
            response.to_json(),
            r#"{"@type":"jsonValueObject","members":[{"@type":"jsonObjectMember","key":"a","value":{"@type":"jsonValueArray","values":[{"@type":"jsonValueNumber","value":1.000000},{"@type":"jsonValueBoolean","value":true},{"@type":"jsonValueNull"},{"@type":"jsonValueString","value":"x"}]}},{"@type":"jsonObjectMember","key":"b","value":{"@type":"jsonValueObject","members":[{"@type":"jsonObjectMember","key":"c","value":{"@type":"jsonValueNumber","value":2.500000}}]}}]}"#
        );
    }

    #[test]
    fn get_json_value_reports_parse_errors() {
        let function = Function::getJsonValue(td_api::getJsonValue {
            json: "{".to_string(),
        });
        let response = run_request(function);
        let Object::error(error) = response else {
            panic!("expected error");
        };
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "Unexpected string end");
    }

    #[test]
    fn get_json_string_round_trips() {
        let value = TdJsonValue::jsonValueString(td_api::jsonValueString {
            value: "hi".to_string(),
        });
        let function = Function::getJsonString(td_api::getJsonString {
            json_value: Some(Box::new(value)),
        });
        let response = run_request(function);
        assert_eq!(response.to_json(), r#"{"@type":"text","text":"\"hi\""}"#);
    }

    #[test]
    fn log_verbosity_level_validation_matches_cpp() {
        let _lock = LOG_TEST_LOCK.lock().unwrap();
        let function = Function::setLogVerbosityLevel(td_api::setLogVerbosityLevel {
            new_verbosity_level: 10000,
        });
        let response = run_request(function);
        assert_eq!(
            response.to_json(),
            r#"{"@type":"error","code":400,"message":"Wrong new verbosity level specified"}"#
        );
        let function = Function::setLogVerbosityLevel(td_api::setLogVerbosityLevel {
            new_verbosity_level: 2,
        });
        assert_eq!(run_request(function).to_json(), r#"{"@type":"ok"}"#);
        tdutils::logging::set_verbosity_level(tdutils::logging::DEBUG + 1);
    }

    #[test]
    fn log_tags_are_the_cpp_list() {
        let function = Function::getLogTags(td_api::getLogTags);
        let response = run_request(function);
        let Object::logTags(tags) = response else {
            panic!("expected logTags");
        };
        assert_eq!(tags.tags.len(), 20);
        // The sorted order of the C++ `std::map<Slice, int *>` iteration.
        assert_eq!(tags.tags[0], "actor");
        assert_eq!(tags.tags[19], "update_file");
        assert_eq!(tags.tags[11], "mtproto");
    }

    #[test]
    fn log_tag_verbosity_validation_matches_cpp() {
        let _lock = LOG_TEST_LOCK.lock().unwrap();
        let function = Function::setLogTagVerbosityLevel(td_api::setLogTagVerbosityLevel {
            tag: "nope".to_string(),
            new_verbosity_level: 3,
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"error","code":400,"message":"Log tag is not found"}"#
        );
        // The C++ per-module default (`int VERBOSITY_NAME(mtproto) =
        // VERBOSITY_NAME(DEBUG) + 7`).
        let function = Function::getLogTagVerbosityLevel(td_api::getLogTagVerbosityLevel {
            tag: "mtproto".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"logVerbosityLevel","verbosity_level":11}"#
        );
        let function = Function::setLogTagVerbosityLevel(td_api::setLogTagVerbosityLevel {
            tag: String::new(),
            new_verbosity_level: 3,
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"error","code":400,"message":"Log tag must be non-empty"}"#
        );
        let function = Function::setLogTagVerbosityLevel(td_api::setLogTagVerbosityLevel {
            tag: "mtproto".to_string(),
            new_verbosity_level: 3,
        });
        assert_eq!(run_request(function).to_json(), r#"{"@type":"ok"}"#);
        let function = Function::getLogTagVerbosityLevel(td_api::getLogTagVerbosityLevel {
            tag: "mtproto".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"logVerbosityLevel","verbosity_level":3}"#
        );
    }

    #[test]
    fn test_return_error_passes_through() {
        let function = Function::testReturnError(td_api::testReturnError {
            error: Some(td_api::Error {
                code: 418,
                message: "teapot".to_string(),
            }),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"error","code":418,"message":"teapot"}"#
        );
    }

    #[test]
    fn log_stream_default_and_empty_round_trip() {
        let _lock = LOG_TEST_LOCK.lock().unwrap();
        tdutils::logging::set_log_interface(None);
        let function = Function::getLogStream(td_api::getLogStream);
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"logStreamDefault"}"#
        );
        let function = Function::setLogStream(td_api::setLogStream {
            log_stream: Some(td_api::LogStream::logStreamEmpty(td_api::logStreamEmpty)),
        });
        assert_eq!(run_request(function).to_json(), r#"{"@type":"ok"}"#);
        let function = Function::getLogStream(td_api::getLogStream);
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"logStreamEmpty"}"#
        );
        // back to the default stream
        let function = Function::setLogStream(td_api::setLogStream {
            log_stream: Some(td_api::LogStream::logStreamDefault(
                td_api::logStreamDefault,
            )),
        });
        assert_eq!(run_request(function).to_json(), r#"{"@type":"ok"}"#);
        let function = Function::getLogStream(td_api::getLogStream);
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"logStreamDefault"}"#
        );
    }

    #[test]
    fn get_file_mime_type_answers_the_canonical_type() {
        // SynchronousRequests.cpp:262-265: `PathView(file_name).extension()`
        // through `MimeType::from_extension`.
        let function = Function::getFileMimeType(td_api::getFileMimeType {
            file_name: "/music/song.mp3".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"text","text":"audio/mpeg"}"#
        );
        let function = Function::getFileMimeType(td_api::getFileMimeType {
            file_name: "archive.tar.gz".to_string(),
        });
        // the bundled mime_types.txt (byte-identical to the C++
        // tdutils/generate/mime_types.txt) has no `gz` entry, so the C++
        // answers the default empty string and logs the unknown extension
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"text","text":""}"#
        );
        let function = Function::getFileMimeType(td_api::getFileMimeType {
            file_name: "noextension".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"text","text":""}"#
        );
    }

    #[test]
    fn get_file_extension_answers_the_canonical_extension() {
        // SynchronousRequests.cpp:267-269: `MimeType::to_extension`.
        let function = Function::getFileExtension(td_api::getFileExtension {
            mime_type: "audio/mpeg".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"text","text":"mp3"}"#
        );
        let function = Function::getFileExtension(td_api::getFileExtension {
            mime_type: "no/such-type".to_string(),
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"text","text":""}"#
        );
    }

    #[test]
    fn search_strings_by_prefix_answers_positions() {
        let function = Function::searchStringsByPrefix(td_api::searchStringsByPrefix {
            strings: vec!["Delta".to_string(), "charlie".to_string()],
            query: "char".to_string(),
            limit: 10,
            return_none_for_empty_query: false,
        });
        assert_eq!(
            run_request(function).to_json(),
            // the C++ keys are the 0-based indices (`hints.add(i, ...)`,
            // misc.cpp:406-419) and the Hints search is prefix-only:
            // "char" prefix-matches only "charlie"
            r#"{"@type":"foundPositions","total_count":1,"positions":[1]}"#
        );
        let function = Function::searchStringsByPrefix(td_api::searchStringsByPrefix {
            strings: vec!["ab1".to_string(), "ab2".to_string(), "ab3".to_string()],
            query: String::new(),
            limit: 2,
            return_none_for_empty_query: false,
        });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"foundPositions","total_count":3,"positions":[0,1]}"#
        );
    }
}
