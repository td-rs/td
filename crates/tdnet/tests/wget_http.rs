//! Ports of the Wget-relevant `test/http.cpp` behaviors plus the S48 plan
//! features: redirects, timeouts, body limits — exercised against a small
//! in-process HTTP server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use tdnet::http_query::HttpQuery;
use tdnet::wget::{WgetParams, wget};
use tdutils::status::Result;

/// What the fake server saw.
struct FakeRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// What the fake server answers.
struct FakeResponse {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl FakeResponse {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }

    fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            reason: "Moved",
            headers: vec![("Location".to_string(), location.to_string())],
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }
}

/// Serves `handler` on a random loopback port with keep-alive.
async fn spawn_server(
    handler: Arc<dyn Fn(FakeRequest) -> FakeResponse + Send + Sync>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let _ = serve(socket, handler).await;
            });
        }
    });
    addr
}

async fn serve(
    mut socket: TcpStream,
    handler: Arc<dyn Fn(FakeRequest) -> FakeResponse + Send + Sync>,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        // one request per keep-alive iteration
        let head_end = loop {
            if let Some(pos) = find_double_crlf(&buf) {
                break pos;
            }
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.push((name, value));
            }
        }
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < content_length {
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            body.extend_from_slice(&chunk[..n]);
        }
        buf.drain(..head_end + 4 + content_length);

        let request = FakeRequest {
            method,
            target,
            headers,
            body,
        };
        let response = handler(request);
        if !response.delay.is_zero() {
            tokio::time::sleep(response.delay).await;
        }

        let mut out = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason);
        for (name, value) in &response.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", response.body.len()));
        out.reserve(response.body.len());
        // one write: two small writes hit Nagle + delayed-ACK (~40ms per request)
        let mut out = out.into_bytes();
        out.extend_from_slice(&response.body);
        socket.write_all(&out).await?;
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn wget_ok(url: &str, params: WgetParams) -> HttpQuery {
    match wget(url, params).await {
        Result::Ok(query) => query,
        Result::Err(status) => panic!("wget failed: {}", status.message()),
    }
}

async fn wget_err(url: &str, params: WgetParams) -> String {
    match wget(url, params).await {
        Result::Err(status) => status.message().to_string(),
        Result::Ok(query) => panic!("expected error, got code {}", query.code),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_ok() {
    let addr = spawn_server(Arc::new(|request| {
        assert_eq!(request.method, "GET");
        FakeResponse::ok(b"hello world".to_vec())
    }))
    .await;
    let query = wget_ok(&format!("http://{addr}/hello"), WgetParams::default()).await;
    assert_eq!(query.code, 200);
    assert_eq!(query.reason, "OK");
    assert_eq!(query.body, b"hello world");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_2xx_is_http_error() {
    let addr = spawn_server(Arc::new(|_| FakeResponse {
        status: 404,
        reason: "Not Found",
        ..FakeResponse::ok(Vec::new())
    }))
    .await;
    let error = wget_err(&format!("http://{addr}/404"), WgetParams::default()).await;
    assert_eq!(error, "HTTP error: 404");
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_chain() {
    let addr = spawn_server(Arc::new(|request| match request.target.as_str() {
        "/r2" => FakeResponse::redirect(302, "/r1"), // relative Location
        "/r1" => FakeResponse::redirect(301, "/hello"), // different 3xx code in the chain
        _ => FakeResponse::ok(b"landed".to_vec()),
    }))
    .await;
    let query = wget_ok(&format!("http://{addr}/r2"), WgetParams::default()).await;
    assert_eq!(query.code, 200);
    assert_eq!(query.body, b"landed");
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_exhaustion_is_http_error() {
    let addr = spawn_server(Arc::new(|_| FakeResponse::redirect(302, "/loop"))).await;
    let error = wget_err(
        &format!("http://{addr}/loop"),
        WgetParams {
            ttl: 3,
            ..WgetParams::default()
        },
    )
    .await;
    assert_eq!(error, "HTTP error: 302");
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_303_is_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let addr = spawn_server(Arc::new(move |request| {
        if request.target == "/never" {
            hits_for_handler.fetch_add(1, Ordering::SeqCst);
        }
        if request.target == "/303" {
            FakeResponse::redirect(303, "/never")
        } else {
            FakeResponse::ok(Vec::new())
        }
    }))
    .await;
    let error = wget_err(&format!("http://{addr}/303"), WgetParams::default()).await;
    assert_eq!(error, "HTTP error: 303");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_308_preserves_method_and_body() {
    let addr = spawn_server(Arc::new(|request| match request.target.as_str() {
        "/redirect-post" => FakeResponse::redirect(308, "/echo"),
        _ => FakeResponse::ok(request.body),
    }))
    .await;
    let query = wget_ok(
        &format!("http://{addr}/redirect-post"),
        WgetParams {
            content: b"the payload".to_vec(),
            content_type: "text/plain".to_string(),
            ..WgetParams::default()
        },
    )
    .await;
    assert_eq!(query.code, 200);
    assert_eq!(query.body, b"the payload");
}

#[tokio::test(flavor = "multi_thread")]
async fn post_sends_content_type() {
    let addr = spawn_server(Arc::new(|request| {
        assert_eq!(request.method, "POST");
        let content_type = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        FakeResponse::ok(content_type.into_bytes())
    }))
    .await;
    let query = wget_ok(
        &format!("http://{addr}/echo"),
        WgetParams {
            content: b"x".to_vec(),
            content_type: "application/json".to_string(),
            ..WgetParams::default()
        },
    )
    .await;
    assert_eq!(query.body, b"application/json");
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_headers_pass_through_and_defaults_apply() {
    let addr = spawn_server(Arc::new(|request| {
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let body = format!(
            "marker={}|encoding={}",
            header("x-marker"),
            header("accept-encoding")
        );
        FakeResponse::ok(body.into_bytes())
    }))
    .await;

    // custom header passes through; the default Accept-Encoding applies
    let query = wget_ok(
        &format!("http://{addr}/headers"),
        WgetParams {
            headers: vec![("X-Marker".to_string(), "test123".to_string())],
            ..WgetParams::default()
        },
    )
    .await;
    let body = String::from_utf8(query.body).unwrap();
    // reqwest normalizes the auto-inserted header value without a space
    assert_eq!(body, "marker=test123|encoding=gzip,deflate");

    // a caller-supplied Accept-Encoding suppresses the default
    let query = wget_ok(
        &format!("http://{addr}/headers"),
        WgetParams {
            headers: vec![("Accept-Encoding".to_string(), "identity".to_string())],
            ..WgetParams::default()
        },
    )
    .await;
    let body = String::from_utf8(query.body).unwrap();
    assert_eq!(body, "marker=|encoding=identity");
}

#[tokio::test(flavor = "multi_thread")]
async fn host_header_defaults_to_authority() {
    let addr = spawn_server(Arc::new(|request| {
        let host = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        FakeResponse::ok(host.into_bytes())
    }))
    .await;
    let query = wget_ok(&format!("http://{addr}/host"), WgetParams::default()).await;
    assert_eq!(String::from_utf8_lossy(&query.body), format!("{addr}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_enforced() {
    let big = vec![b'a'; 64 * 1024];
    let addr = spawn_server(Arc::new(move |_| FakeResponse::ok(big.clone()))).await;

    let error = wget_err(
        &format!("http://{addr}/big"),
        WgetParams {
            max_body_size: Some(1024),
            ..WgetParams::default()
        },
    )
    .await;
    assert_eq!(error, "Response body size limit exceeded");

    // unlimited (the C++ Wget default) reads the whole body
    let query = wget_ok(&format!("http://{addr}/big"), WgetParams::default()).await;
    assert_eq!(query.body.len(), 64 * 1024);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_expires() {
    let addr = spawn_server(Arc::new(|_| FakeResponse {
        delay: Duration::from_secs(1),
        ..FakeResponse::ok(Vec::new())
    }))
    .await;
    let error = wget_err(
        &format!("http://{addr}/slow"),
        WgetParams {
            timeout_in: Duration::from_millis(200),
            ..WgetParams::default()
        },
    )
    .await;
    assert_eq!(error, "Response timeout expired");
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_response_decoded() {
    // A real gzip stream (tdutils' `gzencode` emits a zlib container, exactly
    // like the C++ `gzencode` — both would be rejected as "gzip" here).
    let encoded =
        base64_decode("H4sIAAAAAAAC/8tIzcnJV0jOzy0oSi0uTk3JGOWP8kf5o/xR/iifCD4AdmLr4UAGAAA=");
    let original = "hello compressed".repeat(100);
    let encoded = Arc::new(encoded);
    let addr = spawn_server(Arc::new(move |_| FakeResponse {
        status: 200,
        reason: "OK",
        headers: vec![("Content-Encoding".to_string(), "gzip".to_string())],
        body: Arc::clone(&encoded).as_ref().clone(),
        delay: Duration::ZERO,
    }))
    .await;
    let query = wget_ok(&format!("http://{addr}/gz"), WgetParams::default()).await;
    assert_eq!(String::from_utf8_lossy(&query.body), original);
}

/// Minimal base64 decoder for the embedded fixture (no test-only dep).
fn base64_decode(input: &str) -> Vec<u8> {
    fn value_of(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if let Some(value) = value_of(c) {
            acc = (acc << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn parse_errors_surface_before_any_request() {
    let addr = spawn_server(Arc::new(|_| panic!("server must not be reached"))).await;
    let error = wget_err(&format!("ftp://{addr}/x"), WgetParams::default()).await;
    assert_eq!(error, "Unsupported URL protocol");
    let error = wget_err(&format!("http://{addr}:99999/x"), WgetParams::default()).await;
    assert_eq!(error, "Wrong port number specified in the URL");
}
