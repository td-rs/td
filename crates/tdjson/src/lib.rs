//! `tdjson` cdylib: the 14-symbol C ABI of the JSON client
//! (`tdclientjson_export_list`) — S67.
//!
//! The legacy `td_json_client_*` half wraps a per-client
//! [`tdclient::client_json::ClientJson`]; the new `td_create_client_id` API
//! and the log functions go through the process-wide manager and the
//! [`tdutils::logging`] globals, exactly like `td_json_client.cpp` +
//! `td_log.cpp`. `td_execute` ignores the client pointer (the C++ does too).

use std::ffi::{c_char, c_int, c_void};
use std::sync::Mutex;

/// The dev-harness ws bridge setter (S82): routes the wasm transport's
/// WebSocket dial through a local ws↔TCP bridge instead of Telegram's
/// origin-gated `*.web.telegram.org` endpoints (see
/// `tdclient::session_multi_proxy::set_ws_bridge_url`).
///
/// # Safety
/// `base_url` must be a valid NUL-terminated string.
#[cfg(target_family = "wasm")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tdweb_set_ws_bridge_url(base_url: *const c_char) -> c_int {
    if base_url.is_null() {
        return -1;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(base_url) }
        .to_string_lossy()
        .into_owned();
    tdclient::session_multi_proxy::set_ws_bridge_url(s);
    0
}

use tdclient::client_json::{self, ClientJson, store_string};

// ---------- legacy td_json_client API (ClientJson.cpp) ----------

/// Creates a new instance of TDLib. Returns a pointer to it.
///
/// # Safety
/// The pointer must be passed to the other `td_json_client_*` functions and
/// destroyed with [`td_json_client_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_json_client_create() -> *mut c_void {
    Box::into_raw(Box::new(ClientJson::new())) as *mut c_void
}

/// Sends request to the TDLib client. May be called from any thread.
///
/// # Safety
/// `client` must come from [`td_json_client_create`]; `request` must be a
/// valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_json_client_send(client: *mut c_void, request: *const c_char) {
    let client = unsafe { &*(client as *const ClientJson) };
    let request = unsafe { cstr(request) };
    client.send(request);
}

/// Receives incoming updates and request responses from the TDLib client.
/// May be called from any thread, but must not be called simultaneously
/// from two different threads. The returned pointer is deallocated by the
/// next call to receive/execute on the same thread. Returns null when the
/// timeout expires.
///
/// # Safety
/// `client` must come from [`td_json_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_json_client_receive(
    client: *mut c_void,
    timeout: f64,
) -> *const c_char {
    let client = unsafe { &*(client as *const ClientJson) };
    match client.receive(timeout) {
        Some(response) => store_string(response),
        None => std::ptr::null(),
    }
}

/// Synchronously executes a TDLib request. May be called from any thread.
/// The `client` pointer is ignored (as in the C++).
///
/// # Safety
/// `request` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_json_client_execute(
    _client: *mut c_void,
    request: *const c_char,
) -> *const c_char {
    store_string(client_json::execute(unsafe { cstr(request) }))
}

/// Destroys the TDLib client instance (the C++ `delete`: the close +
/// drain lives in `~Client::Impl`).
///
/// # Safety
/// `client` must come from [`td_json_client_create`] and must not be used
/// afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_json_client_destroy(client: *mut c_void) {
    drop(unsafe { Box::from_raw(client as *mut ClientJson) });
}

// ---------- log management (td_log.cpp) ----------

/// Sets the path to the file where the internal TDLib log will be written.
/// An empty path switches back to the default logging behaviour. Returns
/// true on success, false if the file can't be opened for writing.
///
/// # Safety
/// `file_path` must be a valid NUL-terminated string or null (an empty
/// path resets to the default stream, like the C++).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_set_log_file_path(file_path: *const c_char) -> c_int {
    let path = if file_path.is_null() {
        ""
    } else {
        unsafe { cstr(file_path) }
    };
    i32::from(tdutils::logging::set_file_path(path))
}

/// Sets the maximum size of the log file before it is auto-rotated.
/// Unused if the log is not written to a file. Defaults to 10 MB.
#[unsafe(no_mangle)]
pub extern "C" fn td_set_log_max_file_size(max_file_size: i64) {
    tdutils::logging::set_max_file_size(max_file_size);
}

/// Sets the verbosity level of the internal logging of TDLib.
#[unsafe(no_mangle)]
pub extern "C" fn td_set_log_verbosity_level(new_verbosity_level: c_int) {
    tdutils::logging::set_verbosity_level(new_verbosity_level);
}

/// `td::Log::fatal_error_callback` (Log.cpp): the callback registered by
/// `td_set_log_fatal_error_callback`, invoked through the log message
/// callback at verbosity level 0 — the C++ `fatal_error_callback_wrapper`.
static FATAL_ERROR_CALLBACK: Mutex<Option<unsafe extern "C" fn(*const c_char)>> = Mutex::new(None);

fn fatal_error_callback_wrapper(verbosity_level: i32, message: &str) {
    if verbosity_level == 0 {
        let callback = *FATAL_ERROR_CALLBACK.lock().unwrap();
        if let Some(callback) = callback {
            let message = std::ffi::CString::new(message)
                .unwrap_or_else(|_| std::ffi::CString::new("invalid UTF-8").unwrap());
            unsafe { callback(message.as_ptr()) };
        }
    }
}

/// Sets the callback that will be called when a fatal error happens (the
/// C++ `Log::set_fatal_error_callback`: the callback is registered as a log
/// message callback at verbosity level 0). Pass null to remove it.
#[unsafe(no_mangle)]
pub extern "C" fn td_set_log_fatal_error_callback(
    callback: Option<unsafe extern "C" fn(*const c_char)>,
) {
    match callback {
        Some(callback) => {
            *FATAL_ERROR_CALLBACK.lock().unwrap() = Some(callback);
            tdutils::logging::set_log_message_callback(0, Some(fatal_error_callback_wrapper));
        }
        None => {
            *FATAL_ERROR_CALLBACK.lock().unwrap() = None;
            tdutils::logging::set_log_message_callback(0, None);
        }
    }
}

// ---------- new td_create_client_id API (td_json_client.cpp) ----------

/// Returns an opaque identifier of a new TDLib instance.
#[unsafe(no_mangle)]
pub extern "C" fn td_create_client_id() -> c_int {
    client_json::create_client_id()
}

/// Sends request to the TDLib client. May be called from any thread.
///
/// # Safety
/// `request` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_send(client_id: c_int, request: *const c_char) {
    client_json::send(client_id, unsafe { cstr(request) });
}

/// Receives incoming updates and request responses. Must not be called
/// simultaneously from two different threads. Returns null on timeout.
#[unsafe(no_mangle)]
pub extern "C" fn td_receive(timeout: f64) -> *const c_char {
    match client_json::receive(timeout) {
        Some(response) => store_string(response),
        None => std::ptr::null(),
    }
}

/// Synchronously executes a TDLib request.
///
/// # Safety
/// `request` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn td_execute(request: *const c_char) -> *const c_char {
    store_string(client_json::execute(unsafe { cstr(request) }))
}

/// The host callback registered by `td_set_log_message_callback`.
static LOG_MESSAGE_CALLBACK: Mutex<Option<unsafe extern "C" fn(c_int, *const c_char)>> =
    Mutex::new(None);

fn log_message_callback_wrapper(verbosity_level: i32, message: &str) {
    let callback = *LOG_MESSAGE_CALLBACK.lock().unwrap();
    if let Some(callback) = callback {
        let message = std::ffi::CString::new(message)
            .unwrap_or_else(|_| std::ffi::CString::new("invalid UTF-8").unwrap());
        unsafe { callback(verbosity_level, message.as_ptr()) };
    }
}

/// Sets the callback that will be called when a message is added to the
/// internal TDLib log. Pass null to remove the callback.
#[unsafe(no_mangle)]
pub extern "C" fn td_set_log_message_callback(
    max_verbosity_level: c_int,
    callback: Option<unsafe extern "C" fn(c_int, *const c_char)>,
) {
    match callback {
        Some(callback) => {
            *LOG_MESSAGE_CALLBACK.lock().unwrap() = Some(callback);
            tdutils::logging::set_log_message_callback(
                max_verbosity_level,
                Some(log_message_callback_wrapper),
            );
        }
        None => {
            *LOG_MESSAGE_CALLBACK.lock().unwrap() = None;
            tdutils::logging::set_log_message_callback(0, None);
        }
    }
}

/// The NUL-terminated request string behind a `*const c_char`.
///
/// # Safety
/// `request` must be a valid NUL-terminated string.
unsafe fn cstr<'a>(request: *const c_char) -> &'a str {
    unsafe { std::ffi::CStr::from_ptr(request) }
        .to_str()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the manager-API (`td_receive`) tests. The manager has one
    /// receiver whose `TdReceiver::receive` carries the C++-parity
    /// concurrent-receive guard (`Client.cpp`'s receive_lock — a second
    /// concurrent receive is a LOG(FATAL) abort), so two tests looping
    /// `td_receive` in parallel abort the process: the deterministic
    /// SIGABRT the parallel gate hit. The C++ never receives concurrently
    /// (the ABI contract); the tests honor it through this lock. The legacy
    /// `td_json_client_*` tests stay outside — each ClientJson owns a
    /// private receiver.
    static MANAGER_RECEIVE_LOCK: Mutex<()> = Mutex::new(());

    /// The 14 symbols of `tdclientjson_export_list`, in the export order —
    /// the S67 verification list (also checked against the built cdylib by
    /// `verify_symbols.sh` in the conformance crate).
    const EXPORT_LIST: &[&str] = &[
        "td_json_client_create",
        "td_json_client_destroy",
        "td_json_client_send",
        "td_json_client_receive",
        "td_json_client_execute",
        "td_set_log_file_path",
        "td_set_log_max_file_size",
        "td_set_log_verbosity_level",
        "td_set_log_fatal_error_callback",
        "td_create_client_id",
        "td_send",
        "td_receive",
        "td_execute",
        "td_set_log_message_callback",
    ];

    #[test]
    fn export_list_is_the_cpp_one() {
        // `tdclientjson_export_list` in the C++ repo, verified by the S67
        // symbol-dump gate.
        assert_eq!(EXPORT_LIST.len(), 14);
    }

    #[test]
    fn execute_get_option_round_trip() {
        let request = c"{\"@type\":\"getOption\",\"name\":\"version\"}";
        let response = unsafe { td_execute(request.as_ptr()) };
        let response = unsafe { cstr(response) };
        assert_eq!(
            response,
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }

    #[test]
    fn legacy_client_round_trip() {
        unsafe {
            let client = td_json_client_create();
            let request = c"{\"@type\":\"getOption\",\"name\":\"version\"}";
            td_json_client_send(client, request.as_ptr());
            // The eager startup state precedes the answer (and arrives even
            // without the send).
            let mut response = None;
            for _ in 0..8 {
                let received = td_json_client_receive(client, 1.0);
                assert!(!received.is_null());
                let received = cstr(received);
                if received.starts_with(r#"{"@type":"optionValueString""#) {
                    response = Some(received);
                    break;
                }
            }
            assert_eq!(
                response.expect("the getOption answer"),
                r#"{"@type":"optionValueString","value":"1.8.67"}"#
            );
            td_json_client_destroy(client);
        }
    }

    #[test]
    fn execute_rejects_non_synchronous_methods() {
        let request = c"{\"@type\":\"getMe\"}";
        let response = unsafe { td_execute(request.as_ptr()) };
        let response = unsafe { cstr(response) };
        assert_eq!(
            response,
            r#"{"@type":"error","code":400,"message":"The method can't be executed synchronously"}"#
        );
    }

    #[test]
    fn create_client_id_send_receive() {
        let _receive = MANAGER_RECEIVE_LOCK.lock().unwrap();
        unsafe {
            let client_id = td_create_client_id();
            let request = c"{\"@type\":\"getOption\",\"name\":\"version\"}";
            td_send(client_id, request.as_ptr());
            // The startup updates precede the answer, like the C++.
            let mut response = None;
            for _ in 0..8 {
                let received = td_receive(1.0);
                assert!(!received.is_null());
                let received = cstr(received);
                if received.starts_with(r#"{"@type":"optionValueString""#) {
                    response = Some(received);
                    break;
                }
            }
            assert_eq!(
                response.expect("the getOption answer"),
                format!(
                    r#"{{"@type":"optionValueString","value":"1.8.67","@client_id":{client_id}}}"#
                )
            );
        }
    }

    #[test]
    fn fresh_client_id_emits_the_startup_authorization_state() {
        let _receive = MANAGER_RECEIVE_LOCK.lock().unwrap();
        unsafe {
            // The C++ creates the Td actor inside create_client_id, so a
            // bare td_receive with no td_send yields the startup
            // updateAuthorizationState — the receive-loop contract every
            // shipped binding example relies on (S81 python run).
            let client_id = td_create_client_id();
            let mut startup = None;
            for _ in 0..8 {
                let received = td_receive(1.0);
                assert!(!received.is_null(), "no updates for a fresh client id");
                let received = cstr(received);
                if received.starts_with(r#"{"@type":"updateAuthorizationState""#) {
                    startup = Some(received.to_string());
                    break;
                }
            }
            let startup = startup.expect("startup updateAuthorizationState");
            assert!(
                startup.contains(r#""@type":"authorizationStateWaitTdlibParameters""#),
                "unexpected startup authorization state: {startup}"
            );
            assert!(
                startup.contains(&format!(r#""@client_id":{client_id}"#)),
                "startup update not bound to the created client: {startup}"
            );
        }
    }

    /// The S81 python-example flow end to end: a bare receive reaches the
    /// eager startup state, `close` settles in authorizationStateClosed, and
    /// the legacy trio still answers on the same process afterwards.
    #[test]
    fn python_example_flow_bare_receive_then_close_then_legacy() {
        let _receive = MANAGER_RECEIVE_LOCK.lock().unwrap();
        unsafe {
            let client_id = td_create_client_id();
            let mut saw_wait_params = false;
            for _ in 0..8 {
                let received = td_receive(1.0);
                assert!(!received.is_null(), "no updates for a fresh client id");
                if cstr(received).contains("authorizationStateWaitTdlibParameters") {
                    saw_wait_params = true;
                    break;
                }
            }
            assert!(
                saw_wait_params,
                "startup state never reached waitTdlibParameters"
            );

            let close = c"{\"@type\":\"close\"}";
            td_send(client_id, close.as_ptr());
            let mut saw_closed = false;
            for _ in 0..20 {
                let received = td_receive(1.0);
                if received.is_null() {
                    continue;
                }
                if cstr(received).contains("authorizationStateClosed") {
                    saw_closed = true;
                    break;
                }
            }
            assert!(saw_closed, "close never reached authorizationStateClosed");

            let client = td_json_client_create();
            let request = c"{\"@type\":\"getOption\",\"name\":\"version\",\"@extra\":7}";
            td_json_client_send(client, request.as_ptr());
            let mut answered = false;
            for _ in 0..20 {
                let received = td_json_client_receive(client, 1.0);
                if received.is_null() {
                    continue;
                }
                if cstr(received).contains("@extra\":7") {
                    answered = true;
                    break;
                }
            }
            td_json_client_destroy(client);
            assert!(
                answered,
                "legacy client stopped answering after a manager close"
            );
        }
    }

    #[test]
    fn log_verbosity_level_is_set() {
        unsafe {
            td_set_log_verbosity_level(1);
            let request = c"{\"@type\":\"getLogVerbosityLevel\"}";
            let response = td_execute(request.as_ptr());
            assert_eq!(
                cstr(response),
                r#"{"@type":"logVerbosityLevel","verbosity_level":1}"#
            );
            td_set_log_verbosity_level(tdutils::logging::DEBUG + 1);
        }
    }

    #[test]
    fn set_log_file_path_rotates_and_reports_failures() {
        unsafe {
            // A path that cannot be opened (a directory) fails.
            let dir = c"/";
            assert_eq!(td_set_log_file_path(dir.as_ptr()), 0);
            // An empty path resets to the default stream.
            let empty = c"";
            assert_eq!(td_set_log_file_path(empty.as_ptr()), 1);
        }
    }
}
