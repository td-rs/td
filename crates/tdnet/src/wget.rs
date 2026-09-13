//! Port of `td/net/Wget.h` + `Wget.cpp` on reqwest/hyper.
//!
//! The C++ `Wget` actor becomes an async function with the same request
//! semantics: `parse_url` + charset validation, a `Host` header unless the
//! caller supplied one, `Accept-Encoding: gzip, deflate` (transparently
//! decoded) unless the caller supplied one, whole-exchange timeout
//! (default 10s), up to `ttl` redirects (default 3) on 301/302/307/308 only —
//! 303 and redirect exhaustion surface as `HTTP error: <code>` exactly like
//! the C++ actor — and 2xx resolving to an [`HttpQuery`].
//!
//! Documented deviations from the C++ actor:
//! * Promise → `Result<HttpQuery>`; the caller's future drop is the C++
//!   `tear_down` cancel path.
//! * Redirect `Location` targets are resolved against the base URL (the C++
//!   actor re-parses the header verbatim, so relative targets error there).
//! * IDN hosts are handled by the URL parser's IDNA, replacing `idn_to_ascii`.
//! * TLS is rustls with webpki verification; `VerifyPeer::Off` accepts any
//!   certificate. tdlib's pinned CA store arrives with S50.
//! * `prefer_ipv6` reorders resolved addresses (the C++ code picks the family
//!   in `IPAddress::init_host_port`); DNS caching arrives with S47.
//! * `max_body_size` (None = unlimited, the C++ `Wget` behavior) bounds the
//!   decoded response body — the plan's "body limits" requirement.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crate::http_query::HttpQuery;
use tdutils::http_url::{HttpUrl, parse_url};
use tdutils::status::{Result, Status};

pub use crate::tls::VerifyPeer;

/// `Wget` constructor parameters.
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

/// Resolves names via `getaddrinfo`, reordering the addresses so the
/// preferred family is tried first (`prefer_ipv6`).
struct FamilyResolver {
    prefer_ipv6: bool,
}

impl reqwest::dns::Resolve for FamilyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let prefer_ipv6 = self.prefer_ipv6;
        let name = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            let (preferred, rest): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs
                .into_iter()
                .partition(|addr| addr.is_ipv6() == prefer_ipv6);
            let ordered = preferred.into_iter().chain(rest);
            Ok(Box::new(ordered) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// `Wget::try_init` URL construction: absolute URL without the userinfo
/// (the C++ `HttpHeaderCreator` never sends userinfo either).
fn request_target(url: &HttpUrl) -> String {
    let mut target = String::new();
    target += url.protocol.scheme();
    target += &url.host;
    if url.specified_port > 0 {
        target.push(':');
        target += &url.specified_port.to_string();
    }
    target += &url.query;
    target
}

fn status_from_reqwest(error: reqwest::Error) -> Status {
    Status::error_message(&format!("HTTP request failed: {error}"))
}

/// The `Wget` actor body: fetch `url` and resolve with the response, or the
/// C++ error strings (`Response timeout expired`, `HTTP error: <code>`).
pub async fn wget(url: &str, params: WgetParams) -> Result<HttpQuery> {
    tdutils::try_result!(parsed, parse_url(url));

    let redirect_attempts = Arc::new(AtomicI32::new(params.ttl));
    let attempts_for_policy = Arc::clone(&redirect_attempts);
    let mut builder = reqwest::Client::builder()
        .dns_resolver(Arc::new(FamilyResolver {
            prefer_ipv6: params.prefer_ipv6,
        }))
        // 301/302/307/308 only, at most `ttl` times; everything else stops and
        // is classified by the 2xx check below, like Wget::on_ok.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let follows = matches!(attempt.status().as_u16(), 301 | 302 | 307 | 308);
            if follows && attempts_for_policy.fetch_sub(1, Ordering::SeqCst) > 0 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .gzip(true)
        .deflate(true);
    if params.verify_peer == VerifyPeer::Off {
        builder = builder.danger_accept_invalid_certs(true);
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(e) => {
            return Result::Err(Status::error_message(&format!(
                "Failed to create HTTP client: {e}"
            )));
        }
    };

    let target = request_target(&parsed);
    let request_url = match reqwest::Url::parse(&target) {
        Ok(url) => url,
        Err(e) => {
            return Result::Err(Status::error_message(&format!(
                "Failed to build request URL: {e}"
            )));
        }
    };

    let mut request = if params.content.is_empty() {
        client.get(request_url)
    } else {
        // hc.init_post + set_content_size + set_content_type
        let mut request = client.post(request_url).body(params.content.clone());
        if !params.content_type.is_empty() {
            request = request.header(reqwest::header::CONTENT_TYPE, params.content_type.clone());
        }
        request
    };
    for (name, value) in &params.headers {
        request = request.header(name.as_str(), value.as_str());
    }

    // `?` needs std's Result; the tdutils enum does not implement FromResidual.
    let request_future = async {
        let response = request.send().await.map_err(status_from_reqwest)?;
        let code = response.status().as_u16();
        let reason = response
            .status()
            .canonical_reason()
            .unwrap_or("")
            .to_string();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();

        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(status_from_reqwest)? {
            if let Some(max) = params.max_body_size
                && body.len() + chunk.len() > max
            {
                return Err(Status::error_message("Response body size limit exceeded"));
            }
            body.extend_from_slice(&chunk);
        }

        // Wget::on_ok: 2xx resolves, everything else is an error.
        if (200..300).contains(&code) {
            Ok(HttpQuery {
                code: i32::from(code),
                reason,
                headers,
                body,
            })
        } else {
            Err(Status::error_message(&format!("HTTP error: {code}")))
        }
    };

    // Wget::start_up + timeout_expired: one window covering connection,
    // request and response.
    match tokio::time::timeout(params.timeout_in, request_future).await {
        Ok(result) => result.into(),
        Err(_) => Result::Err(Status::error_message("Response timeout expired")),
    }
}
