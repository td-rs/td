//! wasm32-unknown-emscripten shim for `tdnet::wget`.
//!
//! The native `Wget` rides reqwest+rustls, neither of which links on this
//! target (mio/ring refuse wasm32-unknown-emscripten). The parameter type and
//! the response type are shared with native; the fetch itself is unsupported
//! until the browser transport slice lands (config fetching goes through the
//! `fetch` API there).

use std::time::Duration;

use tdutils::status::{Result, Status};

pub use super::tls::VerifyPeer;

use crate::http_query::HttpQuery;

/// `Wget` constructor parameters — mirrors the native struct field-for-field.
#[derive(Clone, Debug)]
pub struct WgetParams {
    /// Extra request headers; a `Host`/`Accept-Encoding` entry suppresses the
    /// corresponding default, like the C++ `was_host`/`was_accept_encoding`.
    pub headers: Vec<(String, String)>,
    /// `timeout_in` — whole-exchange timeout, C++ default 10 seconds.
    pub timeout_in: Duration,
    /// `ttl` — followed redirects, C++ default 3.
    pub ttl: i32,
    /// `prefer_ipv6`.
    pub prefer_ipv6: bool,
    /// `verify_peer`.
    pub verify_peer: VerifyPeer,
    /// `content` — non-empty switches the request to POST.
    pub content: Vec<u8>,
    /// `content_type` — sent only with non-empty `content`.
    pub content_type: String,
    /// Response body cap on the decoded body; `None` = unlimited (C++ parity).
    pub max_body_size: Option<usize>,
}

impl Default for WgetParams {
    fn default() -> Self {
        Self {
            headers: Vec::new(),
            timeout_in: Duration::from_secs(10),
            ttl: 3,
            prefer_ipv6: false,
            verify_peer: VerifyPeer::On,
            content: Vec::new(),
            content_type: String::new(),
            max_body_size: None,
        }
    }
}

/// The `Wget` actor body — on wasm there is no reqwest backend yet.
pub async fn wget(_url: &str, _params: WgetParams) -> Result<HttpQuery> {
    Result::Err(Status::error_message(
        "HTTP fetch is unsupported on this platform until the browser transport slice",
    ))
}
