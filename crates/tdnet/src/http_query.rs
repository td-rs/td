//! Port of `td/net/HttpQuery.h` — the client-side response subset.
//!
//! C++ `HttpQuery` also carries server-side state (`url_path_`, `args_`,
//! `files_`, form-data); those arrive with the reader-side steps. The
//! client-facing `Wget` surface needs the response code, reason phrase,
//! headers and body.
//!
//! Note: reqwest normalizes response header names to lowercase, so header
//! keys here are lowercase while the C++ class preserves server casing.
//! [`HttpQuery::get_header`] keeps the C++ exact-match semantics.

/// `td::HttpQuery` (response subset).
#[derive(Clone, Debug, Default)]
pub struct HttpQuery {
    /// `code_` — the HTTP status code.
    pub code: i32,
    /// `reason_`.
    pub reason: String,
    /// `headers_` (lowercased keys, in response order).
    pub headers: Vec<(String, String)>,
    /// `content_`.
    pub body: Vec<u8>,
}

impl HttpQuery {
    /// `HttpQuery::get_header` — case-sensitive exact key match, first
    /// occurrence, `None` instead of the C++ empty `Slice`.
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}
