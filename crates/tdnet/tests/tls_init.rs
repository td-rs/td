//! Verification for the fake-TLS port. The C++ upstream check
//! (`test/mtproto.cpp` `Mtproto.tlsinit`) dials `www.google.com:443` live;
//! these tests instead pin the deterministic parts of the hello (record
//! framing, grease bytes, domain placement, the time-stamped
//! `hmac_sha256(secret, data)` MAC) and drive the full handshake against
//! an in-test fake MTProxy that answers with a correctly MACed response —
//! including the fragmented-delivery path the C++ actor's poll loop
//! exercises.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use tdnet::tls_init::{MAX_DOMAIN_LENGTH, connect, generate_header};
use tdutils::crypto::hmac_sha256;
use tdutils::status::Result;

const DOMAIN: &str = "www.google.com";
/// The C++ test's secret: `"0123456789secret"` (16 bytes).
const SECRET: &str = "0123456789secret";

fn error_message(result: Result<TcpStream>) -> String {
    match result {
        Result::Ok(_) => panic!("expected error"),
        Result::Err(status) => status.message().to_string(),
    }
}

#[test]
fn hello_framing_and_mac() {
    let unix_time = 1_234_567;
    let hello = generate_header(DOMAIN, SECRET.as_bytes(), unix_time);

    // Record + handshake scope framing.
    assert_eq!(&hello[..3], b"\x16\x03\x01");
    let record_len = u16::from_be_bytes([hello[3], hello[4]]) as usize;
    assert_eq!(hello.len(), 5 + record_len);
    assert_eq!(&hello[5..7], b"\x01\x00");
    let handshake_len = u16::from_be_bytes([hello[7], hello[8]]) as usize;
    assert_eq!(hello.len(), 9 + handshake_len);
    assert_eq!(&hello[9..11], b"\x03\x03");

    // The client random slot is the MAC: hmac(secret, hello with slot
    // zeroed), with the trailing 4 bytes XORed by the unix time.
    let mut zeroed = hello.clone();
    zeroed[11..43].fill(0);
    let hash = hmac_sha256(SECRET.as_bytes(), &zeroed);
    let stored = &hello[11..43];
    assert_eq!(&hash[..28], &stored[..28]);
    let stamped = i32::from_le_bytes([stored[28], stored[29], stored[30], stored[31]]);
    let hashed = i32::from_le_bytes([hash[28], hash[29], hash[30], hash[31]]);
    assert_eq!(stamped ^ hashed, unix_time, "time stamp mismatch");

    // The domain lands in the SNI extension.
    assert!(
        hello.windows(DOMAIN.len()).any(|w| w == DOMAIN.as_bytes()),
        "domain missing from hello"
    );

    // Grease bytes: `(& 0xF0) + 0x0A` — after the MAC slot (11..43),
    // the one-byte "\x20", the 32 random bytes (44..76) and the
    // "\x00\x20" reserved string, the first grease pair is at 78.
    assert_eq!(hello[78], hello[79]);
    assert_eq!(hello[78] & 0x0F, 0x0A);

    // Padding guarantees a floor; two hellos never coincide.
    assert!(hello.len() >= 517);
    let second = generate_header(DOMAIN, SECRET.as_bytes(), unix_time);
    assert_ne!(hello, second, "hello randomness collapsed");
}

#[test]
fn domain_is_truncated_to_max_length() {
    let long_domain = "a".repeat(300);
    let hello = generate_header(&long_domain, SECRET.as_bytes(), 0);
    let mut max_run = 0usize;
    let mut run = 0usize;
    for byte in &hello {
        run = if *byte == b'a' { run + 1 } else { 0 };
        max_run = max_run.max(run);
    }
    assert_eq!(max_run, MAX_DOMAIN_LENGTH);
}

#[tokio::test]
async fn tls_init_tunnel_with_fake_mtproxy() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Read the hello: 5-byte record header + record body.
        let mut header = [0u8; 5];
        sock.read_exact(&mut header).await.unwrap();
        let record_len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut record = vec![0u8; record_len];
        sock.read_exact(&mut record).await.unwrap();
        let mut received = header.to_vec();
        received.extend_from_slice(&record);
        let hello_rand: [u8; 32] = received[11..43].try_into().unwrap();

        // Build the response: two framed sections with the MAC at
        // offset 11 over (hello_rand ++ response-with-zeroed-slot).
        let mut response = Vec::new();
        response.extend_from_slice(b"\x16\x03\x03\x00\x30");
        response.extend_from_slice(&[0xABu8; 48]);
        response.extend_from_slice(b"\x14\x03\x03\x00\x01\x01\x17\x03\x03\x00\x40");
        response.extend_from_slice(&[0xCDu8; 64]);
        response[11..43].fill(0);
        let mut mac_input = Vec::with_capacity(32 + response.len());
        mac_input.extend_from_slice(&hello_rand);
        mac_input.extend_from_slice(&response);
        let mac = hmac_sha256(SECRET.as_bytes(), &mac_input);
        response[11..43].copy_from_slice(&mac);

        // Fragmented delivery — the C++ poll loop re-entry points.
        for chunk in response.chunks(7) {
            sock.write_all(chunk).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Tunnel established: echo.
        let mut buf = [0u8; 1024];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sock.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut tunneled = connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        DOMAIN,
        SECRET,
        0.0,
    )
    .await
    .unwrap();
    tunneled.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tunneled.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    server.abort();
}

#[tokio::test]
async fn response_hash_mismatch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut header = [0u8; 5];
        sock.read_exact(&mut header).await.unwrap();
        let record_len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut record = vec![0u8; record_len];
        sock.read_exact(&mut record).await.unwrap();
        let mut received = header.to_vec();
        received.extend_from_slice(&record);
        let hello_rand: [u8; 32] = received[11..43].try_into().unwrap();

        let mut response = Vec::new();
        response.extend_from_slice(b"\x16\x03\x03\x00\x30");
        response.extend_from_slice(&[0xABu8; 48]);
        response.extend_from_slice(b"\x14\x03\x03\x00\x01\x01\x17\x03\x03\x00\x40");
        response.extend_from_slice(&[0xCDu8; 64]);
        response[11..43].fill(0);
        let mut mac_input = Vec::with_capacity(32 + response.len());
        mac_input.extend_from_slice(&hello_rand);
        mac_input.extend_from_slice(&response);
        let mut mac = hmac_sha256(SECRET.as_bytes(), &mac_input);
        mac[0] ^= 0xFF;
        response[11..43].copy_from_slice(&mac);
        sock.write_all(&response).await.unwrap();
    });

    let error = error_message(
        connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            DOMAIN,
            SECRET,
            0.0,
        )
        .await,
    );
    assert_eq!(error, "Response hash mismatch");
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_response_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut header = [0u8; 5];
        sock.read_exact(&mut header).await.unwrap();
        let record_len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut record = vec![0u8; record_len];
        sock.read_exact(&mut record).await.unwrap();
        sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .unwrap();
    });

    let error = error_message(
        connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            DOMAIN,
            SECRET,
            0.0,
        )
        .await,
    );
    assert_eq!(error, "First part of response to hello is invalid");
    server.await.unwrap();
}
