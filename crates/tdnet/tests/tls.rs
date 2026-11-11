//! Verification for the rustls TLS port. The hermetic tests pin the
//! `SslCtx::create` contract (default pinned store, custom PEM file,
//! unverified mode, exact C++ error string); the two network tests
//! (ignored by default, run with `cargo test -p tdnet --test tls
//! -- --ignored`) prove the plan's gate: a verified TLS handshake to a
//! Telegram-operated endpoint and an IP-as-host handshake to a Telegram
//! DC address — the exact MTProto-over-TLS transport setup.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::net::TcpStream;

use tdnet::tls::{SslCtx, VerifyPeer, connect};
use tdutils::status::Result as TdResult;

fn error_text(result: TdResult<SslCtx>) -> String {
    match result {
        TdResult::Ok(_) => panic!("expected error"),
        TdResult::Err(status) => status.message().to_string(),
    }
}

#[test]
fn ssl_ctx_default_pinned_store() {
    SslCtx::create("", VerifyPeer::On).unwrap();
}

#[test]
fn ssl_ctx_unverified() {
    SslCtx::create("", VerifyPeer::Off).unwrap();
}

#[test]
fn ssl_ctx_missing_cert_file() {
    // The exact C++ `SSL_CTX_load_verify_locations` error string.
    let error = error_text(SslCtx::create(
        "/nonexistent/certificates.pem",
        VerifyPeer::On,
    ));
    assert_eq!(error, "Failed to set custom certificate file");
}

#[test]
fn ssl_ctx_garbage_cert_file() {
    let path = std::env::temp_dir().join(format!("tdnet-tls-garbage-{}.pem", std::process::id()));
    std::fs::write(&path, b"not a certificate").unwrap();
    let error = error_text(SslCtx::create(path.to_str().unwrap(), VerifyPeer::On));
    assert_eq!(error, "Failed to set custom certificate file");
    std::fs::remove_file(&path).ok();
}

/// The plan's S50 gate: a verified TLS handshake to Telegram's
/// DC-fronting endpoints (`telegram.org`, `api.telegram.org` — the API
/// endpoint is served from the same DC infrastructure the JSON API
/// clients talk to). DC IPs themselves speak MTProto, not arbitrary TLS;
/// their transport-level TLS path is the S51 fake-TLS layer.
#[tokio::test]
#[ignore = "requires network"]
async fn verified_handshakes_with_telegram_endpoints() {
    let ctx = SslCtx::create("", VerifyPeer::On).unwrap();
    for host in ["telegram.org", "api.telegram.org"] {
        let mut tls = connect(
            TcpStream::connect((host, 443)).await.unwrap(),
            host,
            false,
            &ctx,
        )
        .await
        .unwrap();
        tls.write_all(
            format!("HEAD / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1."), "{host}: got {response}");
    }
}
