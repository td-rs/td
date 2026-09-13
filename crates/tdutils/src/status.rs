//! Port of `td/utils/Status.h` + `td/utils/Status.cpp`.
//!
//! A [`Status`] is either `OK` or an error carrying an [`ErrorType`], a
//! 23-bit error code and a message. [`Result`] wraps `std::result::Result`
//! with tdlib's move/prefix/suffix API on top, and the `TRY_STATUS` /
//! `TRY_RESULT` macros are provided as [`try_status!`] / [`try_result!`].

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;

/// Error code bounds of the C++ `Info::error_code` bitfield (23 bits, signed).
pub const MIN_ERROR_CODE: i32 = -(1 << 22) + 1;
pub const MAX_ERROR_CODE: i32 = (1 << 22) - 1;

/// `td::Status::ErrorType` — where the error code came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorType {
    /// `td::Status::Error(code, message)`.
    General,
    /// `td::Status::PosixError(errno, message)` (or `WindowsError`).
    Os,
}

/// `td::strerror_safe`: the C `strerror` text for `code`, without the
/// `(os error N)` suffix Rust's `io::Error` appends.
pub fn strerror_safe(code: i32) -> String {
    let error = std::io::Error::from_raw_os_error(code);
    let text = error.to_string();
    match text.strip_suffix(&format!(" (os error {code})")) {
        Some(prefix) => prefix.to_string(),
        None => text,
    }
}

#[derive(Debug)]
struct ErrorInfo {
    static_flag: bool,
    error_type: ErrorType,
    error_code: i32,
    message: String,
}

/// `td::Status`. Cloning an error is cheap and shares the message; the
/// `static_flag` distinction of the C++ class is preserved but only
/// observable through [`Status::is_static`].
#[derive(Clone)]
pub struct Status {
    info: Option<Arc<ErrorInfo>>,
}

impl Status {
    /// `td::Status::OK()`.
    pub fn ok() -> Self {
        Self { info: None }
    }

    /// `td::Status::Error(err, message)`: a general error with an arbitrary
    /// message. The code is clamped to the 23-bit range like in C++.
    pub fn error(code: i32, message: &str) -> Self {
        Self::with_info(ErrorType::General, code, message.to_string())
    }

    /// `td::Status::Error(Slice message)`: `Error(0, message)`.
    pub fn error_message(message: &str) -> Self {
        Self::error(0, message)
    }

    /// `td::Status::PosixError(saved_errno, message)`.
    pub fn posix_error(saved_errno: i32, message: &str) -> Self {
        Self::with_info(ErrorType::Os, saved_errno, message.to_string())
    }

    /// `td::Status::Error<Code>()`: a static, per-code error singleton with an
    /// empty message (the `static_flag` path in C++). Statics in generic
    /// scopes are hoisted to module scope in Rust, so the per-code instances
    /// live in a process-wide registry instead; each stays alive forever,
    /// exactly like the C++ template statics.
    pub fn error_code<const CODE: i32>() -> Self {
        use std::collections::BTreeMap;
        use std::sync::Mutex;

        static MAP: OnceLock<Mutex<BTreeMap<i32, Arc<ErrorInfo>>>> = OnceLock::new();
        let map = MAP.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut map = map.lock().unwrap();
        let info = map.entry(CODE).or_insert_with(|| {
            Arc::new(ErrorInfo {
                static_flag: true,
                error_type: ErrorType::General,
                error_code: CODE,
                message: String::new(),
            })
        });
        Self {
            info: Some(Arc::clone(info)),
        }
    }

    fn with_info(error_type: ErrorType, error_code: i32, message: String) -> Self {
        let error_code = error_code.clamp(MIN_ERROR_CODE, MAX_ERROR_CODE);
        Self {
            info: Some(Arc::new(ErrorInfo {
                static_flag: false,
                error_type,
                error_code,
                message,
            })),
        }
    }

    fn info(&self) -> &ErrorInfo {
        self.info.as_deref().expect("info() called on an OK Status")
    }

    /// `td::Status::is_static` — true for `OK` and for `Error<Code>()`
    /// singletons.
    pub fn is_static(&self) -> bool {
        self.info.as_deref().is_none_or(|info| info.static_flag)
    }

    pub fn is_ok(&self) -> bool {
        self.info.is_none()
    }

    pub fn is_error(&self) -> bool {
        self.info.is_some()
    }

    /// `td::Status::code()` — 0 for `OK`.
    pub fn code(&self) -> i32 {
        self.info.as_deref().map_or(0, |info| info.error_code)
    }

    /// `td::Status::message()` — `"OK"` for `OK`.
    pub fn message(&self) -> &str {
        self.info
            .as_deref()
            .map_or("OK", |info| info.message.as_str())
    }

    /// `td::Status::public_message()` — the message suitable for a user;
    /// `PosixError`s report the `strerror` text instead of the message.
    pub fn public_message(&self) -> Cow<'_, str> {
        match &self.info {
            None => Cow::Borrowed("OK"),
            Some(info) if info.error_type == ErrorType::Os => {
                Cow::Owned(strerror_safe(info.error_code))
            }
            Some(info) => Cow::Borrowed(&info.message),
        }
    }

    /// `td::Status::ensure()` — panics unless `OK`.
    pub fn ensure(&self) {
        if !self.is_ok() {
            panic!("Unexpected Status {self}");
        }
    }

    /// `td::Status::ensure_error()` — panics unless an error.
    pub fn ensure_error(&self) {
        if self.is_ok() {
            panic!("Unexpected Status::OK");
        }
    }

    /// `td::Status::move_as_error_prefix(prefix)`: `prefix` + this message,
    /// with this code and type.
    pub fn move_as_error_prefix(&self, prefix: &str) -> Self {
        assert!(self.is_error());
        self.move_as_error_prefix_unsafe(prefix)
    }

    pub fn move_as_error_prefix_unsafe(&self, prefix: &str) -> Self {
        let info = self.info();
        Self::with_info(
            info.error_type,
            info.error_code,
            format!("{prefix}{}", info.message),
        )
    }

    /// `td::Status::move_as_error_prefix(status)`: `status`'s code and type,
    /// with `status.message()` + this message.
    pub fn move_as_error_prefix_status(&self, status: &Status) -> Self {
        status.move_as_error_suffix(self.message())
    }

    /// `td::Status::move_as_error_suffix(suffix)`: this message + `suffix`,
    /// with this code and type.
    pub fn move_as_error_suffix(&self, suffix: &str) -> Self {
        assert!(self.is_error());
        self.move_as_error_suffix_unsafe(suffix)
    }

    pub fn move_as_error_suffix_unsafe(&self, suffix: &str) -> Self {
        let info = self.info();
        Self::with_info(
            info.error_type,
            info.error_code,
            format!("{}{suffix}", info.message),
        )
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::ok()
    }
}

impl fmt::Display for Status {
    /// `td::Status::print`: `OK`, `[Error : code : message]` or
    /// `[PosixError : strerror : code : message]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(info) = &self.info else {
            return f.write_str("OK");
        };
        match info.error_type {
            ErrorType::General => {
                write!(f, "[Error : {} : {}]", info.error_code, info.message)
            }
            ErrorType::Os => write!(
                f,
                "[PosixError : {} : {} : {}]",
                strerror_safe(info.error_code),
                info.error_code,
                info.message
            ),
        }
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// `td::Result<T>`: the value or a [`Status`], with tdlib's move-oriented
/// accessors on top. Kept as an enum so the [`try_result!`] macro can match
/// on it directly.
#[derive(Debug)]
pub enum Result<T> {
    Ok(T),
    Err(Status),
}

impl<T> Result<T> {
    /// `td::Result<T>::Result(Status &&)`: requires an error status
    /// (the C++ class asserts this for every `T` except `Unit`).
    pub fn from_status(status: Status) -> Self {
        assert!(status.is_error(), "Result constructed from OK Status");
        Self::Err(status)
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    /// `td::Result::error()` — panics unless this is an error.
    pub fn error(&self) -> &Status {
        match self {
            Self::Err(status) => status,
            Self::Ok(_) => panic!("error() on an OK Result"),
        }
    }

    /// `td::Result::move_as_error()` — takes the error, leaving this result
    /// as `Error<-4>`.
    pub fn move_as_error(&mut self) -> Status {
        match self {
            Self::Err(status) => {
                let status = status.clone();
                *self = Self::from_status(Status::error_code::<-4>());
                status
            }
            Self::Ok(_) => panic!("move_as_error() on an OK Result"),
        }
    }

    pub fn move_as_error_unsafe(&mut self) -> Status {
        self.move_as_error()
    }

    pub fn move_as_error_prefix(&mut self, prefix: &str) -> Status {
        self.move_as_error().move_as_error_prefix_unsafe(prefix)
    }

    pub fn move_as_error_prefix_unsafe(&mut self, prefix: &str) -> Status {
        self.move_as_error_unsafe()
            .move_as_error_prefix_unsafe(prefix)
    }

    pub fn move_as_error_prefix_status(&mut self, prefix: &Status) -> Status {
        self.move_as_error().move_as_error_prefix_status(prefix)
    }

    pub fn move_as_error_suffix(&mut self, suffix: &str) -> Status {
        self.move_as_error().move_as_error_suffix_unsafe(suffix)
    }

    pub fn move_as_error_suffix_unsafe(&mut self, suffix: &str) -> Status {
        self.move_as_error_unsafe()
            .move_as_error_suffix_unsafe(suffix)
    }

    /// `td::Result::ok()` — panics unless this is OK.
    pub fn ok(&self) -> &T {
        match self {
            Self::Ok(value) => value,
            Self::Err(status) => panic!("ok() on an error Result: {status}"),
        }
    }

    /// `td::Result::ok_ref()` — mutably borrows the value, panicking on error.
    pub fn ok_ref(&mut self) -> &mut T {
        match self {
            Self::Ok(value) => value,
            Self::Err(status) => panic!("ok_ref() on an error Result: {status}"),
        }
    }

    /// `td::Result::move_as_ok()` — takes the value, panicking on error.
    pub fn move_as_ok(self) -> T {
        match self {
            Self::Ok(value) => value,
            Self::Err(status) => panic!("move_as_ok() on an error Result: {status}"),
        }
    }

    /// `std::result::Result::unwrap` — takes the value, panicking on error.
    pub fn unwrap(self) -> T {
        self.move_as_ok()
    }

    /// `std::result::Result::unwrap_or_else` — takes the value, or maps the
    /// error through `f`.
    pub fn unwrap_or_else(self, f: impl FnOnce(Status) -> T) -> T {
        match self {
            Self::Ok(value) => value,
            Self::Err(status) => f(status),
        }
    }

    /// `td::Result::move_as_ok_unsafe()` — takes the value without checking.
    pub fn move_as_ok_unsafe(self) -> T {
        match self {
            Self::Ok(value) => value,
            Self::Err(_) => panic!("move_as_ok_unsafe() on an error Result"),
        }
    }

    /// `td::Result::clear()`.
    pub fn clear(&mut self) {
        *self = Self::from_status(Status::error_code::<-1>());
    }

    /// `td::Result::move_map(f)`: `f(value)` wrapped in a new [`Result`].
    pub fn move_map<U>(self, f: impl FnOnce(T) -> U) -> Result<U> {
        match self {
            Self::Ok(value) => Result::Ok(f(value)),
            Self::Err(status) => Result::Err(status),
        }
    }

    /// `td::Result::move_fmap(f)`: returns `f(value)` directly on success and
    /// the error converted to `U` otherwise; `f`'s return type must accept a
    /// [`Status`] (in C++ it converts implicitly, typically to `Result<U>`).
    pub fn move_fmap<U: FromStatus>(self, f: impl FnOnce(T) -> U) -> U {
        match self {
            Self::Ok(value) => f(value),
            Self::Err(status) => U::from_status(status),
        }
    }

    /// Converts to the underlying `std::result::Result`.
    pub fn into_inner(self) -> std::result::Result<T, Status> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(status) => Err(status),
        }
    }
}

impl<T: Clone> Clone for Result<T> {
    /// `td::Result::clone()`.
    fn clone(&self) -> Self {
        match self {
            Self::Ok(value) => Self::Ok(value.clone()),
            Self::Err(status) => Self::Err(status.clone()),
        }
    }
}

impl<T> From<T> for Result<T> {
    /// Wraps a value; a [`Status`] value must go through
    /// [`FromStatus::from_status`] instead (as `Status&&` picks the error
    /// constructor in C++).
    fn from(value: T) -> Self {
        Self::Ok(value)
    }
}

impl<T> From<std::result::Result<T, Status>> for Result<T> {
    fn from(result: std::result::Result<T, Status>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(status) => Self::Err(status),
        }
    }
}

/// Conversion from a [`Status`] into a result-like type — the Rust side of
/// the C++ implicit `Status` → `Result<T>` conversion used by `TRY_STATUS`
/// and [`Result::move_fmap`].
pub trait FromStatus {
    fn from_status(status: Status) -> Self;
}

impl FromStatus for Status {
    fn from_status(status: Status) -> Self {
        status
    }
}

impl<T> FromStatus for Result<T> {
    /// Only errors convert; an `OK` status panics, as in the C++ constructor.
    fn from_status(status: Status) -> Self {
        Result::from_status(status)
    }
}

impl<T: fmt::Display> fmt::Display for Result<T> {
    /// `td::operator<<` for `Result`: `Ok{value}` or the error.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok(value) => write!(f, "Ok{{{value}}}"),
            Self::Err(status) => fmt::Display::fmt(status, f),
        }
    }
}

/// `td::TRY_STATUS(status)`: on error, returns it converted to the enclosing
/// function's return type ([`Status`] or [`Result`]).
#[macro_export]
macro_rules! try_status {
    ($status:expr) => {{
        let try_status = $status;
        if try_status.is_error() {
            return $crate::status::FromStatus::from_status(try_status);
        }
    }};
    ($status:expr, $prefix:expr) => {{
        let try_status = $status;
        if try_status.is_error() {
            return $crate::status::FromStatus::from_status(
                try_status.move_as_error_prefix_unsafe($prefix),
            );
        }
    }};
}

/// `td::TRY_RESULT(name, result)`: binds `name` to the unwrapped value or
/// returns the error.
#[macro_export]
macro_rules! try_result {
    ($name:ident, $result:expr) => {
        let $name = match $result {
            $crate::status::Result::Ok(__value) => __value,
            $crate::status::Result::Err(__status) => {
                return $crate::status::FromStatus::from_status(__status);
            }
        };
    };
    ($name:ident, $result:expr, $prefix:expr) => {
        let $name = match $result {
            $crate::status::Result::Ok(__value) => __value,
            $crate::status::Result::Err(__status) => {
                return $crate::status::FromStatus::from_status(
                    __status.move_as_error_prefix_unsafe($prefix),
                );
            }
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status::error(123, "test message")
    }

    #[test]
    fn ok_status_is_ok_and_static() {
        let ok = Status::ok();
        assert!(ok.is_ok());
        assert!(!ok.is_error());
        assert!(ok.is_static());
        assert_eq!(ok.code(), 0);
        assert_eq!(ok.message(), "OK");
        assert_eq!(ok.public_message(), "OK");
        assert_eq!(ok.to_string(), "OK");
    }

    #[test]
    fn error_status_carries_code_type_and_message() {
        let error = status();
        assert!(!error.is_ok());
        assert!(error.is_error());
        assert!(!error.is_static());
        assert_eq!(error.code(), 123);
        assert_eq!(error.message(), "test message");
        assert_eq!(error.public_message(), "test message");
        assert_eq!(error.to_string(), "[Error : 123 : test message]");
    }

    #[test]
    fn error_code_is_clamped_to_the_23_bit_range() {
        assert_eq!(Status::error(i32::MAX, "").code(), MAX_ERROR_CODE);
        assert_eq!(Status::error(i32::MIN, "").code(), MIN_ERROR_CODE);
        assert_eq!(Status::error(0, "").code(), 0);
    }

    #[test]
    fn error_message_uses_code_zero() {
        let error = Status::error_message("boom");
        assert_eq!(error.code(), 0);
        assert_eq!(error.message(), "boom");
    }

    #[test]
    fn static_errors_are_singletons_per_code() {
        let a = Status::error_code::<7>();
        let b = Status::error_code::<7>();
        let c = Status::error_code::<8>();
        assert!(a.is_static());
        assert!(a.is_error());
        assert_eq!(a.code(), 7);
        assert_eq!(a.message(), "");
        assert!(Arc::ptr_eq(
            a.info.as_ref().unwrap(),
            b.info.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            a.info.as_ref().unwrap(),
            c.info.as_ref().unwrap()
        ));
        assert_eq!(c.code(), 8);
    }

    #[test]
    fn clone_preserves_static_flag() {
        let error = status().clone();
        assert!(!error.is_static());
        assert_eq!(error.to_string(), "[Error : 123 : test message]");
        let static_error = Status::error_code::<5>().clone();
        assert!(static_error.is_static());
        assert_eq!(static_error.code(), 5);
    }

    #[test]
    fn posix_error_prints_strerror_and_public_message() {
        // ENOENT = 2 on Linux.
        let error = Status::posix_error(2, "open failed");
        assert_eq!(error.code(), 2);
        assert_eq!(error.message(), "open failed");
        assert_eq!(error.public_message(), "No such file or directory");
        assert_eq!(
            error.to_string(),
            "[PosixError : No such file or directory : 2 : open failed]"
        );
    }

    #[test]
    fn prefix_and_suffix_concatenate_the_message() {
        let error = status();
        let prefixed = error.move_as_error_prefix("prefix ");
        assert_eq!(prefixed.code(), 123);
        assert_eq!(prefixed.to_string(), "[Error : 123 : prefix test message]");

        let suffixed = error.move_as_error_suffix(" suffix");
        assert_eq!(suffixed.to_string(), "[Error : 123 : test message suffix]");

        let prefixed = error.move_as_error_prefix("x");
        let chained = prefixed.move_as_error_suffix("y");
        assert_eq!(chained.to_string(), "[Error : 123 : xtest messagey]");
    }

    #[test]
    fn prefix_preserves_os_error_type() {
        let error = Status::posix_error(2, "open failed");
        let prefixed = error.move_as_error_prefix("step 1: ");
        assert_eq!(
            prefixed.to_string(),
            "[PosixError : No such file or directory : 2 : step 1: open failed]"
        );
    }

    #[test]
    fn prefix_with_status_uses_the_other_status_code() {
        let outer = Status::error(1, "outer");
        let inner = Status::error(2, "inner");
        let prefixed = inner.move_as_error_prefix_status(&outer);
        assert_eq!(prefixed.code(), 1);
        assert_eq!(prefixed.to_string(), "[Error : 1 : outerinner]");
    }

    #[test]
    #[should_panic(expected = "Unexpected Status")]
    fn ensure_panics_on_error() {
        status().ensure();
    }

    #[test]
    #[should_panic(expected = "Unexpected Status::OK")]
    fn ensure_error_panics_on_ok() {
        Status::ok().ensure_error();
    }

    #[test]
    fn result_ok_and_error_paths() {
        let ok: Result<i32> = Result::from(42);
        assert!(ok.is_ok());
        assert_eq!(*ok.ok(), 42);
        assert_eq!(ok.move_as_ok(), 42);

        let mut error: Result<i32> = Result::from_status(status());
        assert!(error.is_error());
        assert_eq!(error.error().code(), 123);
        assert_eq!(error.move_as_error().code(), 123);
        assert_eq!(error.error().code(), -4, "moved-out result holds Error<-4>");
    }

    #[test]
    fn result_move_map_and_move_fmap() {
        let ok: Result<i32> = Result::from(7);
        let mapped = ok.move_map(|value| value * 2);
        assert_eq!(mapped.move_as_ok(), 14);

        let ok: Result<i32> = Result::from(7);
        let mapped = ok.move_fmap(|value| Result::from(value * 2));
        assert_eq!(mapped.move_as_ok(), 14);

        let error: Result<i32> = Result::from_status(status());
        let mapped = error.move_fmap(|value| Result::from(value * 2));
        assert!(mapped.is_error());
        assert_eq!(mapped.error().code(), 123);
    }

    #[test]
    fn result_clear_resets_to_error_minus_one() {
        let mut ok: Result<i32> = Result::from(42);
        ok.clear();
        assert!(ok.is_error());
        assert_eq!(ok.error().code(), -1);
    }

    #[test]
    fn try_status_macro_returns_errors() {
        fn check(value: i32) -> Result<i32> {
            try_status!(make(value));
            Result::from(0)
        }
        fn make(value: i32) -> Status {
            if value == 0 {
                Status::ok()
            } else {
                Status::error(value, "bad")
            }
        }

        assert_eq!(check(0).move_as_ok(), 0);
        assert_eq!(check(3).error().code(), 3);
    }

    #[test]
    fn try_result_macro_binds_and_propagates() {
        fn check(value: i32) -> Result<i32> {
            try_result!(bound, make(value));
            Result::from(bound)
        }
        fn make(value: i32) -> Result<i32> {
            if value == 0 {
                Result::from(0)
            } else {
                Result::from_status(Status::error(value, "bad"))
            }
        }

        assert_eq!(check(0).move_as_ok(), 0);
        assert_eq!(check(3).error().code(), 3);
    }

    #[test]
    fn result_display_matches_cpp_format() {
        let ok: Result<i32> = Result::from(5);
        assert_eq!(ok.to_string(), "Ok{5}");
        let error: Result<i32> = Result::from_status(Status::error(1, "x"));
        assert_eq!(error.to_string(), "[Error : 1 : x]");
    }
}
