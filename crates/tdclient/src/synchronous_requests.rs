//! Port of `td/telegram/SynchronousRequests.{h,cpp}` + the td_api half of
//! `td/telegram/JsonValue.cpp` (S68): the `td_execute` offline request set.
//!
//! The log- and JSON-related functions are complete here, the M.8
//! MessageEntity slice flipped the five text-parse functions
//! (`getTextEntities`, `parseTextEntities`, `parseMarkdown`,
//! `getMarkdownText` — the C++ `parseHtml` is the `parseTextEntities`
//! HTML parse mode), and M.24 flips `getLanguagePackString` (the
//! `SynchronousRequests::do_request` tail over the language pack
//! database). The phone-number and push-id functions still land with
//! their managers (Wave 3); until then they report "The method can't be
//! executed synchronously", like any other request.

use std::sync::Mutex;

use crate::message_entity;
use crate::option_manager;
use crate::td_api;
use crate::td_api::{Function, JsonValue as TdJsonValue, Object};
use tdutils::json::{JsonValue, json_decode};
use tdutils::status::{Result, Status};

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
        Function::getTextEntities(_)
            | Function::parseTextEntities(_)
            | Function::parseMarkdown(_)
            | Function::getMarkdownText(_)
            | Function::getJsonValue(_)
            | Function::getJsonString(_)
            | Function::getThemeParametersJsonString(_)
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
            | Function::getLanguagePackString(_)
            | Function::getChatFolderDefaultIconName(_)
            | Function::getPushReceiverId(_)
    ) || matches!(function, Function::getOption(g) if option_manager::is_synchronous_option(&g.name))
    // Not ported yet (need their managers): searchQuote,
    // checkQuickReplyShortcutName, getCountryFlagEmoji, cleanFileName,
    // getPhoneNumberInfoSync.
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
        Function::getLanguagePackString(request) => {
            // `SynchronousRequests::do_request` (SynchronousRequests.cpp:276-280)
            // — the static LanguagePackManager lookup over the path-keyed
            // language pack database (M.24)
            crate::language_pack_manager::get_language_pack_string(
                &request.language_pack_database_path,
                &request.localization_target,
                &request.language_pack_id,
                &request.key,
            )
        }
        Function::getChatFolderDefaultIconName(request) => {
            let folder = match request.folder {
                Some(f) => f,
                None => return make_error(400, "Chat folder must be non-empty"),
            };
            let icon_name =
                crate::dialog_filter_manager::DialogFilter::get_default_icon_name(&folder);
            Object::chatFolderIcon(td_api::ChatFolderIcon { name: icon_name })
        }
        Function::getPushReceiverId(request) => {
            // `SynchronousRequests::do_request` (SynchronousRequests.cpp:288-303)
            // — the NotificationManager payload walk; the VLOG(notifications)
            // error log rides the result arm.
            match crate::notification_manager::get_push_receiver_id(&request.payload) {
                Ok(receiver_id) => {
                    Object::pushReceiverId(td_api::PushReceiverId { id: receiver_id })
                }
                Err((code, message)) => make_error(code, &message),
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
        // `SynchronousRequests::do_request(getThemeParametersJsonString)`
        // (SynchronousRequests.cpp:325-329) — the pure
        // `ThemeManager::get_theme_parameters_json_string` static
        Function::getThemeParametersJsonString(request) => {
            let default_parameters = td_api::ThemeParameters::default();
            let theme_parameters = request.theme.as_ref().unwrap_or(&default_parameters);
            Object::text(td_api::Text {
                text: crate::theme_manager::get_theme_parameters_json_string(theme_parameters),
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
        Function::getTextEntities(request) => {
            if !tdutils::utf8::check_utf8(request.text.as_bytes()) {
                return make_error(400, "Text must be encoded in UTF-8");
            }
            let text_entities = message_entity::find_entities(&request.text, false, false);
            Object::textEntities(td_api::TextEntities {
                entities: message_entity::get_text_entities_object::<()>(
                    None,
                    &text_entities,
                    false,
                    i32::MAX,
                ),
            })
        }
        Function::parseTextEntities(request) => {
            // must not use clean_input_string, because \r may be used as a separator
            if !tdutils::utf8::check_utf8(request.text.as_bytes()) {
                return make_error(400, "Text must be encoded in UTF-8");
            }

            let parse_mode_version = match request.parse_mode.as_ref() {
                None => return make_error(400, "Parse mode must be non-empty"),
                Some(td_api::TextParseMode::textParseModeHTML(_)) => None,
                Some(td_api::TextParseMode::textParseModeMarkdown(markdown)) => {
                    Some(markdown.version)
                }
            };
            let mut text = request.text;
            let r_entities: std::result::Result<Vec<message_entity::MessageEntity>, Status> = {
                if tdutils::utf8::utf8_length(&text) > 65536 {
                    Err(Status::error_message("Text is too long"))
                } else {
                    match parse_mode_version {
                        None => message_entity::parse_html(&mut text),
                        Some(version) if version == 0 || version == 1 => {
                            message_entity::parse_markdown(&mut text)
                        }
                        Some(2) => message_entity::parse_markdown_v2(&mut text),
                        Some(_) => Err(Status::error_message("Wrong Markdown version specified")),
                    }
                }
            };
            let entities = match r_entities {
                std::result::Result::Ok(entities) => entities,
                std::result::Result::Err(status) => {
                    return make_error(400, &format!("Can't parse entities: {}", status.message()));
                }
            };

            Object::formattedText(td_api::FormattedText {
                text,
                entities: message_entity::get_text_entities_object::<()>(
                    None, &entities, false, -1,
                ),
            })
        }
        Function::parseMarkdown(request) => {
            // The null-text 400 normally arrives through the JSON null
            // bridge (`null_pointer_request_error`); this is the same
            // `SynchronousRequests.cpp:188` check for in-model callers.
            let Some(request_text) = request.text else {
                return make_error(400, "Text must be non-empty");
            };
            let mut text = request_text.text;
            let entities =
                match message_entity::get_message_entities::<()>(None, request_text.entities, true)
                {
                    std::result::Result::Ok(entities) => entities,
                    std::result::Result::Err(status) => return make_error(400, status.message()),
                };
            let mut entities = entities;
            let status = message_entity::fix_formatted_text(
                &mut text,
                &mut entities,
                true,
                true,
                true,
                true,
                true,
                true,
                None,
            );
            if let std::result::Result::Err(status) = status {
                return make_error(400, status.message());
            }

            let mut parsed_text =
                message_entity::parse_markdown_v3(message_entity::FormattedText { text, entities });
            message_entity::fix_formatted_text(
                &mut parsed_text.text,
                &mut parsed_text.entities,
                true,
                true,
                true,
                true,
                true,
                true,
                None,
            )
            .expect("the C++ .ensure()");
            Object::formattedText(message_entity::get_formatted_text_object::<()>(
                None,
                &parsed_text,
                false,
                i32::MAX,
            ))
        }
        Function::getMarkdownText(request) => {
            // `SynchronousRequests.cpp:215` via the null bridge; the
            // in-model check for programmatic callers.
            let Some(request_text) = request.text else {
                return make_error(400, "Text must be non-empty");
            };
            let mut text = request_text.text;
            let entities = match message_entity::get_message_entities::<()>(
                None,
                request_text.entities,
                false,
            ) {
                std::result::Result::Ok(entities) => entities,
                std::result::Result::Err(status) => return make_error(400, status.message()),
            };
            let mut entities = entities;
            let status = message_entity::fix_formatted_text(
                &mut text,
                &mut entities,
                true,
                true,
                true,
                true,
                true,
                true,
                None,
            );
            if let std::result::Result::Err(status) = status {
                return make_error(400, status.message());
            }

            let markdown_text =
                message_entity::get_markdown_v3(message_entity::FormattedText { text, entities });
            Object::formattedText(message_entity::get_formatted_text_object::<()>(
                None,
                &markdown_text,
                false,
                i32::MAX,
            ))
        }
        _ => make_error(400, "The method can't be executed synchronously"),
    }
}

/// The flattened-model null bridge. The generated td_api model (S09)
/// decodes a null or absent object field to its default variant, while the
/// C++ keeps `object_ptr` nulls and checks them in
/// `SynchronousRequests::do_request`. The checks that change the observable
/// output cannot be expressed in `run_request` (the nullness is already
/// lost), so `client_json.rs::to_request` applies them at the JSON
/// boundary, where every request passes. The returned `testReturnError`
/// serializes to the same `td_api::error` the C++ `make_error` response
/// does, keeping the output byte-identical (including the `@extra` splice).
///
/// `SynchronousRequests.cpp:188/215`: `parseMarkdown`/`getMarkdownText`
/// require a non-null `text`; `SynchronousRequests.cpp:153`:
/// `parseTextEntities` requires a non-null `parse_mode`.
pub fn null_pointer_request_error(request: &JsonValue) -> Option<Function> {
    let JsonValue::Object(object) = request else {
        return None;
    };
    let type_name = match object.get_field("@type") {
        Some(JsonValue::String(name)) => name.as_str(),
        _ => return None,
    };
    let null_or_absent =
        |field: &str| matches!(object.get_field(field), None | Some(JsonValue::Null));
    let error = |message: &str| {
        Some(Function::testReturnError(td_api::testReturnError {
            error: Some(td_api::Error {
                code: 400,
                message: message.to_string(),
            }),
        }))
    };
    match type_name {
        "parseMarkdown" | "getMarkdownText" if null_or_absent("text") => {
            error("Text must be non-empty")
        }
        "parseTextEntities" if null_or_absent("parse_mode") => {
            error("Parse mode must be non-empty")
        }
        "getChatFolderDefaultIconName" if null_or_absent("folder") => {
            error("Chat folder must be non-empty")
        }
        _ => None,
    }
}

/// `MessageEntity.cpp:3673` (`get_message_entities`): input entities with a
/// null `type_` are skipped — a null element, or an element whose `type`
/// field is absent or null. The flattened model decodes them to a default
/// `textEntityTypeMention` and keeps them, so the skip is applied to the
/// raw JSON at the boundary instead; the C++ decodes faithfully and drops
/// them one step later, and the observable output is identical.
/// Wrong-shaped elements (numbers, strings, objects with a non-null but
/// invalid `type`) are left untouched so the decoder still reports the
/// C++'s 400 messages. Only the two flipped functions taking input
/// entities are covered — `searchQuote` stays in the deferred set, and
/// M.9/M.10 consumers extend this when they wire their
/// `get_message_entities` inputs.
pub fn drop_null_typed_entities(request: &mut JsonValue) {
    let JsonValue::Object(object) = request else {
        return;
    };
    let type_name = match object.get_field("@type") {
        Some(JsonValue::String(name)) => name.as_str(),
        _ => return,
    };
    if type_name != "parseMarkdown" && type_name != "getMarkdownText" {
        return;
    }
    let Some(JsonValue::Object(text)) = object.get_mut_field("text") else {
        return;
    };
    let Some(JsonValue::Array(entities)) = text.get_mut_field("entities") else {
        return;
    };
    entities.retain(|entity| match entity {
        JsonValue::Null => false,
        JsonValue::Object(entity_object) => !matches!(
            entity_object.get_field("type"),
            None | Some(JsonValue::Null)
        ),
        _ => true,
    });
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

    /// The M.8 differential gate: the recorded C++ `td_execute` outputs
    /// (tdlib at the pinned upstream commit d1085f9ce, the local
    /// TD_BUILD/libtdjson.so 1.8.67 build) replayed through the ported
    /// `td_execute` path must produce byte-identical JSON. The corpus
    /// covers URL/mention/hashtag/cashtag/bank-card/timestamp matchers,
    /// Markdown v1/v2/v3 and HTML parsing, and their error paths.
    ///
    /// The M.8.6 Markdown v1 battery hand-corrects the six bare-bracket
    /// `[url]` entries: `fd53a8160` ("Replace text in-place in
    /// parse_markdown") made the v1 parser compact `text` in place, and the
    /// bare-bracket branch `url.assign(text, begin_pos + 1,
    /// i - begin_pos - 1)` (MessageEntity.cpp:2005-2007) then reads bytes
    /// the compaction already overwrote — a read-after-overwrite bug in the
    /// pinned C++ itself (probe: "[t.me]" yields the `TextUrl`
    /// `"http://.mee/"` from "t.me"). Every version before that refactor —
    /// and the Rust port — reads the pristine bracket content, so those six
    /// entries hold the source-true answers instead of the mangled ones.
    ///
    /// The M.8.7 Markdown v2 battery is the next 76 entries (nested
    /// entities, escapes, pre/code narrowing, blockquotes and expandable
    /// blockquotes, tg://user/tg://emoji/tg://time links, CRLF handling,
    /// UTF-16 offsets and the 400 error paths). The M.8.8 Markdown v3
    /// battery is the next 97 entries: parseMarkdown with and without
    /// pre-existing entities, getMarkdownText over every entity type the
    /// v3 formatter handles plus the kept-entity and return-original
    /// paths, the 400 error paths and parse→format→parse round-trip
    /// pairs. The M.8.9 HTML battery is the last 612 entries:
    /// parseTextEntities in textParseModeHTML mode over the C++ unit-test
    /// battery (every tag alias, the attribute subset, entity decoding,
    /// pre/code merging, tg-emoji/tg-time/blockquote forms, UTF-16
    /// offsets and the 400 error paths), a malformed-HTML fuzz seed set
    /// (truncated tags, stray brackets, numeric-entity overflow, attribute
    /// edge cases, case folding) and byte-prefix truncations of six
    /// representative inputs. The M.8.10 battery is the last 42 entries:
    /// the flipped functions' null-field edge cases (null/absent `text`
    /// and `parse_mode` 400s, null-typed input entities skipped like
    /// `get_message_entities` does, wrong-typed fields, unknown and
    /// numeric `@type`s, the `@extra` splice through the null bridge),
    /// entity-heavy `getTextEntities` kitchen-sink texts and
    /// parseMarkdown/getMarkdownText round-trips. Corpus responses store
    /// the oracle output
    /// decoded: tdlib's
    /// JSON layer escapes non-ASCII as \uXXXX (astral chars as surrogate
    /// pairs) while the Rust layer emits raw UTF-8, so non-ASCII was
    /// normalized to characters at recording time (control-char escapes
    /// stay, matching both layers).
    #[test]
    fn message_entity_requests_match_the_cpp_oracle() {
        use tdutils::json::{JsonValue, json_decode};

        let golden = include_str!("testdata/message_entity_cpp_golden.jsonl");
        let mut checked = 0usize;
        for line in golden.lines() {
            let record = match json_decode(line) {
                Result::Ok(value) => value,
                Result::Err(status) => panic!("bad golden line: {}", status.message()),
            };
            let JsonValue::Object(record_object) = &record else {
                panic!("the golden line is not an object")
            };
            let JsonValue::String(request) =
                record_object.get_field("request").expect("request field")
            else {
                panic!("the golden request is not a string")
            };
            let JsonValue::String(expected_response) =
                record_object.get_field("response").expect("response field")
            else {
                panic!("the golden response is not a string")
            };
            let actual = crate::client_json::execute(request);
            assert_eq!(
                actual, *expected_response,
                "diverged on request: {request}\n  expected: {}\n  actual: {}",
                *expected_response, actual
            );
            checked += 1;
        }
        assert_eq!(checked, 1164, "the corpus shrank");
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

    #[test]
    fn get_chat_folder_default_icon_name_resolves_icons() {
        let mut folder = td_api::ChatFolder {
            name: None,
            icon: None,
            color_id: -1,
            is_shareable: false,
            pinned_chat_ids: Vec::new(),
            included_chat_ids: Vec::new(),
            excluded_chat_ids: Vec::new(),
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            include_contacts: true,
            include_non_contacts: false,
            include_bots: false,
            include_groups: false,
            include_channels: false,
        };
        let function =
            Function::getChatFolderDefaultIconName(td_api::getChatFolderDefaultIconName {
                folder: Some(folder.clone()),
            });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"chatFolderIcon","name":"Private"}"#
        );

        folder.include_contacts = false;
        folder.include_groups = true;
        let function =
            Function::getChatFolderDefaultIconName(td_api::getChatFolderDefaultIconName {
                folder: Some(folder),
            });
        assert_eq!(
            run_request(function).to_json(),
            r#"{"@type":"chatFolderIcon","name":"Groups"}"#
        );

        let empty_function =
            Function::getChatFolderDefaultIconName(td_api::getChatFolderDefaultIconName {
                folder: None,
            });
        assert_eq!(
            run_request(empty_function).to_json(),
            r#"{"@type":"error","code":400,"message":"Chat folder must be non-empty"}"#
        );
    }
}
