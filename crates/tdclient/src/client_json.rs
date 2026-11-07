//! Port of `td/telegram/ClientJson.{h,cpp}` (S68): the JSON ↔ td_api
//! boundary of the C ABI. Request parsing matches the C++ exactly — parse
//! errors become a `testReturnError` request carrying the 400 error, so
//! they flow through the normal response path (`get_return_error_function`).

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::client::{Client, ClientManager, Request, Response};
use crate::td_api;
use crate::td_api::{Function, Object};
use tdutils::json::{JsonValue, json_decode, json_encode};
use tdutils::status::Result;

/// `ClientJson.cpp:get_return_error_function`.
fn return_error_function(error_message: String) -> Function {
    Function::testReturnError(td_api::testReturnError {
        error: Some(td_api::Error {
            code: 400,
            message: error_message,
        }),
    })
}

/// `ClientJson.cpp:to_request`: decode the request, extract `@extra`
/// (re-encoded verbatim into the response), then `from_json` the rest.
/// Returns `(function, extra)`.
pub fn to_request(request: &str) -> (Function, String) {
    let mut json_value = match json_decode(request) {
        Result::Ok(value) => value,
        Result::Err(status) => {
            return (
                return_error_function(format!(
                    "Failed to parse request as JSON object: {}",
                    status.message()
                )),
                String::new(),
            );
        }
    };
    if json_value.ty() != tdutils::json::JsonValueType::Object {
        return (
            return_error_function("Expected a JSON object".to_string()),
            String::new(),
        );
    }

    let extra = match &mut json_value {
        JsonValue::Object(object) => {
            if object.has_field("@extra") {
                json_encode(&object.extract_field("@extra"), false)
            } else {
                String::new()
            }
        }
        _ => unreachable!(),
    };

    // The flattened-model null bridge (see synchronous_requests.rs): the
    // C++ keeps null object fields as null pointers and checks them in
    // `SynchronousRequests::do_request` / `get_message_entities`; the
    // generated model decodes them to default values, so the checks that
    // change the observable output are applied to the raw JSON here.
    crate::synchronous_requests::drop_null_typed_entities(&mut json_value);
    if let Some(error_function) =
        crate::synchronous_requests::null_pointer_request_error(&json_value)
    {
        return (error_function, extra);
    }

    // The generated parser takes the re-serialized object (the C++ moves
    // the tdutils value into `from_json`; the equivalent here is one
    // encode/parse round).
    let function = match Function::from_json(&json_encode(&json_value, false)) {
        std::result::Result::Ok(function) => function,
        std::result::Result::Err(status) => {
            return (
                return_error_function(format!(
                    "Failed to parse JSON object as TDLib request: {status}"
                )),
                extra,
            );
        }
    };
    (function, extra)
}

/// `ClientJson.cpp:from_response`: serialize the object, replace the
/// trailing `}` with `,"@extra":<extra>` (when present) and
/// `,"@client_id":<id>` (when non-zero).
pub fn from_response(object: &Object, extra: &str, client_id: i32) -> String {
    let mut buf = object.to_json();
    debug_assert!(!buf.is_empty() && buf.ends_with('}'));
    buf.pop();
    if !extra.is_empty() {
        buf.push_str(",\"@extra\":");
        buf.push_str(extra);
    }
    if client_id != 0 {
        buf.push_str(",\"@client_id\":");
        buf.push_str(&client_id.to_string());
    }
    buf.push('}');
    buf
}

thread_local! {
    /// `ClientJson.cpp`'s thread-local output (`TD_THREAD_LOCAL string
    /// *current_output`): valid until the next receive/execute on the same
    /// thread.
    static CURRENT_OUTPUT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// `ClientJson.cpp:store_string`.
pub fn store_string(string: String) -> *const c_char {
    CURRENT_OUTPUT.with(|output| {
        let mut output = output.borrow_mut();
        output.clear();
        output.push_str(&string);
        // A Rust `String` is not NUL-terminated (the C++ `std::string`
        // c_str is) — terminate explicitly; the buffer stays valid until
        // the next store on this thread (the C ABI contract).
        output.push('\0');
        output.as_ptr() as *const c_char
    })
}

/// `td::ClientJson` — the legacy `td_json_client_*` interface: a private
/// client plus the per-client `@extra` map keyed by an atomic id starting
/// at 1 (the C++ `extra_id_`).
pub struct ClientJson {
    client: Client,
    extra: Mutex<HashMap<u64, String>>,
    extra_id: AtomicU64,
}

impl Default for ClientJson {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientJson {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            extra: Mutex::new(HashMap::new()),
            extra_id: AtomicU64::new(1),
        }
    }

    /// `ClientJson::send`.
    pub fn send(&self, request: &str) {
        let (function, extra) = to_request(request);
        let extra_id = self.extra_id.fetch_add(1, Ordering::Relaxed);
        if !extra.is_empty() {
            self.extra.lock().unwrap().insert(extra_id, extra);
        }
        self.client.send(Request {
            id: extra_id,
            function,
        });
    }

    /// `ClientJson::receive`: returns the response JSON, or `None` on
    /// timeout (the C++ null pointer).
    pub fn receive(&self, timeout: f64) -> Option<String> {
        let response = self.client.receive(timeout)?;
        self.format_response(response, 0)
    }

    /// `ClientJson::execute`.
    pub fn execute(&self, request: &str) -> String {
        let (function, extra) = to_request(request);
        let object = Client::execute(Request { id: 0, function });
        from_response(&object, &extra, 0)
    }

    fn format_response(&self, response: Response, client_id_override: i32) -> Option<String> {
        let object = response.object?;
        let mut extra = String::new();
        if response.request_id != 0 {
            let mut extras = self.extra.lock().unwrap();
            if let Some(stored) = extras.remove(&response.request_id) {
                extra = stored;
            }
        }
        Some(from_response(&object, &extra, client_id_override))
    }
}

// The new `td_create_client_id` API (`ClientJson.cpp` static half): a
// process-wide extra map + the manager singleton.

static EXTRA: std::sync::LazyLock<Mutex<HashMap<u64, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static EXTRA_ID: AtomicU64 = AtomicU64::new(1);

/// `json_create_client_id`.
pub fn create_client_id() -> i32 {
    get_manager().create_client_id()
}

/// `json_send`.
pub fn send(client_id: i32, request: &str) {
    let (function, extra) = to_request(request);
    let request_id = EXTRA_ID.fetch_add(1, Ordering::Relaxed);
    if !extra.is_empty() {
        EXTRA.lock().unwrap().insert(request_id, extra);
    }
    get_manager().send(client_id, request_id, function);
}

/// `json_receive`.
pub fn receive(timeout: f64) -> Option<String> {
    let response = get_manager().receive(timeout);
    let object = response.object?;
    let mut extra = String::new();
    if response.request_id != 0 {
        let mut extras = EXTRA.lock().unwrap();
        if let Some(stored) = extras.remove(&response.request_id) {
            extra = stored;
        }
    }
    Some(from_response(&object, &extra, response.client_id))
}

/// `json_execute`.
pub fn execute(request: &str) -> String {
    let (function, extra) = to_request(request);
    from_response(&ClientManager::execute(function), &extra, 0)
}

/// `ClientJson.cpp:get_manager`.
pub fn get_manager() -> &'static ClientManager {
    crate::client::get_manager_singleton()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_request_extracts_extra_and_parses() {
        let (function, extra) =
            to_request(r#"{"@type":"getOption","name":"version","@extra":1.5}"#);
        assert_eq!(
            function,
            Function::getOption(td_api::getOption {
                name: "version".to_string()
            })
        );
        assert_eq!(extra, "1.5");
    }

    #[test]
    fn to_request_reports_json_syntax_errors_like_cpp() {
        let (function, extra) = to_request("{");
        let Function::testReturnError(request) = function else {
            panic!("expected testReturnError");
        };
        let error = request.error.expect("error");
        assert_eq!(error.code, 400);
        assert!(
            error
                .message
                .starts_with("Failed to parse request as JSON object: "),
            "got: {}",
            error.message
        );
        assert!(extra.is_empty());
    }

    #[test]
    fn to_request_rejects_non_objects() {
        let (function, _) = to_request("[1,2]");
        let Function::testReturnError(request) = function else {
            panic!("expected testReturnError");
        };
        assert_eq!(
            request.error.expect("error").message,
            "Expected a JSON object"
        );
    }

    #[test]
    fn to_request_reports_unknown_functions() {
        let (function, extra) = to_request(r#"{"@type":"nope"}"#);
        let Function::testReturnError(request) = function else {
            panic!("expected testReturnError");
        };
        assert_eq!(
            request.error.expect("error").message,
            "Failed to parse JSON object as TDLib request: Unknown class \"nope\""
        );
        assert!(extra.is_empty());
    }

    #[test]
    fn from_response_appends_extra_and_client_id() {
        let object = Object::error(td_api::Error {
            code: 400,
            message: "Bad Request: test".to_string(),
        });
        assert_eq!(
            from_response(&object, "", 0),
            r#"{"@type":"error","code":400,"message":"Bad Request: test"}"#
        );
        assert_eq!(
            from_response(&object, "{\"a\":1}", 0),
            r#"{"@type":"error","code":400,"message":"Bad Request: test","@extra":{"a":1}}"#
        );
        assert_eq!(
            from_response(&object, "", 42),
            r#"{"@type":"error","code":400,"message":"Bad Request: test","@client_id":42}"#
        );
        assert_eq!(
            from_response(&object, "\"x\"", 42),
            r#"{"@type":"error","code":400,"message":"Bad Request: test","@extra":"x","@client_id":42}"#
        );
    }

    #[test]
    fn from_response_escapes_non_ascii_like_the_cpp_json_string() {
        // The tdjson output surface serializes strings through td's
        // JsonString (tdutils/td/utils/JsonBuilder.cpp:60): named escapes
        // for \" \\ \b \f \n \r \t, other controls as lowercase \uXXXX,
        // EVERY non-ASCII codepoint as lowercase \uXXXX with astral chars
        // as UTF-16 surrogate pairs, and DEL (0x7f) raw. Byte-pinned to the
        // S12 oracle sample (conformance/tl_json_oracle/dump_json.cpp
        // "error"), the evidence class the M.8 golden round-trip could
        // not provide (issue #72).
        let object = Object::error(td_api::Error {
            code: -42,
            message: "line1\n\"quoted\" \\ \u{1} é中😀 tab\tend\u{8}\u{c}\r \u{7f} del".to_string(),
        });
        assert_eq!(
            from_response(&object, "", 0),
            "{\"@type\":\"error\",\"code\":-42,\"message\":\"line1\\n\\\"quoted\\\" \\\\ \\u0001 \\u00e9\\u4e2d\\ud83d\\ude00 tab\\tend\\b\\f\\r \u{7f} del\"}"
        );
    }

    #[test]
    fn legacy_client_execute_round_trips() {
        let client = ClientJson::new();
        assert_eq!(
            client.execute(r#"{"@type":"getOption","name":"version"}"#),
            r#"{"@type":"optionValueString","value":"1.8.68"}"#
        );
        assert_eq!(
            client.execute(r#"{"@type":"getOption","name":"version","@extra":[1]}"#),
            r#"{"@type":"optionValueString","value":"1.8.68","@extra":[1]}"#
        );
    }

    #[test]
    fn legacy_client_send_receive_preserves_extra() {
        let client = ClientJson::new();
        client.send(r#"{"@type":"getOption","name":"version","@extra":{"n":2}}"#);
        // The startup updates (updateOption version/commit_hash + the auth
        // state) precede the answer, like the C++.
        let mut response = None;
        for _ in 0..8 {
            match client.receive(0.3) {
                Some(received) if received.starts_with(r#"{"@type":"optionValueString""#) => {
                    response = Some(received);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert_eq!(
            response.as_deref(),
            Some(r#"{"@type":"optionValueString","value":"1.8.68","@extra":{"n":2}}"#)
        );
    }

    #[test]
    fn new_api_send_receive_round_trips_with_client_id() {
        let client_id = create_client_id();
        send(client_id, r#"{"@type":"getOption","name":"version"}"#);
        let mut response = None;
        for _ in 0..8 {
            match receive(0.3) {
                Some(received) if received.starts_with(r#"{"@type":"optionValueString""#) => {
                    response = Some(received);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert_eq!(
            response,
            Some(format!(
                r#"{{"@type":"optionValueString","value":"1.8.68","@client_id":{client_id}}}"#
            ))
        );
    }

    #[test]
    fn execute_is_process_wide() {
        assert_eq!(
            execute(r#"{"@type":"getOption","name":"version"}"#),
            r#"{"@type":"optionValueString","value":"1.8.68"}"#
        );
    }
}
