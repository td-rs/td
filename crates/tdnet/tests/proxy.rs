//! Ports of the C++ proxy tests. Upstream's only SOCKS5 gate
//! (`test/mtproto.cpp` `Mtproto.socks5`) is disabled (`return;` up front)
//! and dials a dead proxy IP, so these drive the ported connect functions
//! against in-test fake SOCKS5 / HTTP-CONNECT servers covering: both auth
//! modes, wrong credentials, fragmented replies (the C++ actor's poll
//! re-entry points), protocol error codes, IPv6 targets, and the
//! "Proxy has sent too many data" contract.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use tdnet::{http_proxy_connect, socks5_connect};
use tdutils::status::Result;

async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                pump(&mut sock).await;
            });
        }
    });
    addr
}

/// Binds the proxy-side listener. The caller moves the listener into its
/// server task, so client connects queue in the kernel until `accept()`
/// runs — no spawn race.
async fn bind_proxy() -> (SocketAddr, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, listener)
}

async fn read_exact_n(sock: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    sock.read_exact(&mut buf).await.unwrap();
    buf
}

/// The established-tunnel behavior: bytes in, bytes back out.
async fn pump(sock: &mut TcpStream) {
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
}

async fn round_trip(tunneled: &mut TcpStream) {
    tunneled.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tunneled.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}

fn error_message(result: Result<TcpStream>) -> String {
    match result {
        Result::Ok(_) => panic!("expected error"),
        Result::Err(status) => status.message().to_string(),
    }
}

#[tokio::test]
async fn socks5_no_auth_tunnel() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        // Greeting: no username → exactly one method, no-auth.
        assert_eq!(read_exact_n(&mut sock, 3).await, vec![0x05, 0x01, 0x00]);
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        // Request: VER CMD RSV ATYP addr port for the IPv4 target.
        let request = read_exact_n(&mut sock, 10).await;
        assert_eq!(&request[..4], &[0x05, 0x01, 0x00, 0x01]);
        let expected = match target {
            SocketAddr::V4(v4) => v4.ip().octets(),
            _ => panic!("expected v4 echo target"),
        };
        assert_eq!(&request[4..8], &expected);
        assert_eq!(&request[8..10], &target.port().to_be_bytes());
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        pump(&mut sock).await;
    });

    let mut tunneled = socks5_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        target,
        "",
        "",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn socks5_username_password_tunnel() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        assert_eq!(
            read_exact_n(&mut sock, 4).await,
            vec![0x05, 0x02, 0x00, 0x02]
        );
        sock.write_all(&[0x05, 0x02]).await.unwrap();
        // RFC 1929 subnegotiation.
        let auth = read_exact_n(&mut sock, 11).await;
        assert_eq!(auth[0], 0x01);
        assert_eq!(auth[1], 4);
        assert_eq!(&auth[2..6], b"user");
        assert_eq!(auth[6], 4);
        assert_eq!(&auth[7..11], b"pass");
        sock.write_all(&[0x01, 0x00]).await.unwrap();
        let request = read_exact_n(&mut sock, 10).await;
        assert_eq!(&request[..4], &[0x05, 0x01, 0x00, 0x01]);
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        pump(&mut sock).await;
    });

    let mut tunneled = socks5_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        target,
        "user",
        "pass",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn socks5_ipv6_target() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        assert_eq!(read_exact_n(&mut sock, 3).await, vec![0x05, 0x01, 0x00]);
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        // ATYP=4, 16 address bytes, 2 port bytes — target is ::1.
        let request = read_exact_n(&mut sock, 22).await;
        assert_eq!(&request[..4], &[0x05, 0x01, 0x00, 0x04]);
        let mut expected_v6 = [0u8; 16];
        expected_v6[15] = 1;
        assert_eq!(&request[4..20], &expected_v6);
        sock.write_all(&[0x05u8, 0x00, 0x00, 0x04]).await.unwrap();
        sock.write_all(&[0u8; 18]).await.unwrap();
        pump(&mut sock).await;
    });

    let v6_target = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        target.port(),
    );
    let mut tunneled = socks5_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        v6_target,
        "",
        "",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn socks5_fragmented_replies() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        read_exact_n(&mut sock, 3).await;
        // Byte-by-byte replies exercise the wait-for-availability paths.
        for byte in [0x05u8, 0x00] {
            sock.write_all(&[byte]).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        read_exact_n(&mut sock, 10).await;
        for byte in [0x05u8, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0] {
            sock.write_all(&[byte]).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        pump(&mut sock).await;
    });

    let mut tunneled = socks5_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        target,
        "",
        "",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn socks5_unsupported_auth_mode() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        read_exact_n(&mut sock, 3).await;
        sock.write_all(&[0x05, 0xFF]).await.unwrap();
    });
    let error = error_message(
        socks5_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "",
            "",
        )
        .await,
    );
    assert_eq!(error, "Unsupported authentication mode");
    server.await.unwrap();
}

#[tokio::test]
async fn socks5_wrong_password() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        read_exact_n(&mut sock, 4).await;
        sock.write_all(&[0x05, 0x02]).await.unwrap();
        read_exact_n(&mut sock, 11).await;
        sock.write_all(&[0x01, 0x01]).await.unwrap();
    });
    let error = error_message(
        socks5_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "user",
            "pass",
        )
        .await,
    );
    assert_eq!(error, "Wrong username or password");
    server.await.unwrap();
}

#[tokio::test]
async fn socks5_server_error_code() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        read_exact_n(&mut sock, 3).await;
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        read_exact_n(&mut sock, 10).await;
        // Reply 0x05 0x01 (general failure) ...
        sock.write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
    });
    let error = error_message(
        socks5_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "",
            "",
        )
        .await,
    );
    assert_eq!(error, "Receive error code 1 from server");
    server.await.unwrap();
}

#[tokio::test]
async fn socks5_early_data_rejected() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        read_exact_n(&mut sock, 3).await;
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        read_exact_n(&mut sock, 10).await;
        // Valid reply plus one junk byte in the same segment.
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0xAA])
            .await
            .unwrap();
    });
    let error = error_message(
        socks5_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "",
            "",
        )
        .await,
    );
    assert_eq!(error, "Proxy has sent too many data");
    server.await.unwrap();
}

#[tokio::test]
async fn http_connect_tunnel() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let expected_request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let request = read_exact_n(&mut sock, expected_request.len()).await;
        assert_eq!(String::from_utf8(request).unwrap(), expected_request);
        // Fragmented reply across the actor's poll re-entry points.
        for chunk in ["HTTP/1.1 ", "200 Connection est", "ablished\r\n\r\n"] {
            sock.write_all(chunk.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        pump(&mut sock).await;
    });

    let mut tunneled = http_proxy_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        target,
        "",
        "",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn http_connect_with_credentials() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let expected_request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n\r\n"
    );
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let request = read_exact_n(&mut sock, expected_request.len()).await;
        assert_eq!(String::from_utf8(request).unwrap(), expected_request);
        sock.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await.unwrap();
        pump(&mut sock).await;
    });

    let mut tunneled = http_proxy_connect(
        TcpStream::connect(proxy_addr).await.unwrap(),
        target,
        "user",
        "pass",
    )
    .await
    .unwrap();
    round_trip(&mut tunneled).await;
    server.abort();
}

#[tokio::test]
async fn http_connect_refused() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 4096];
        let _ = sock.read(&mut buf).await.unwrap();
        sock.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            .await
            .unwrap();
    });
    let error = error_message(
        http_proxy_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "",
            "",
        )
        .await,
    );
    assert_eq!(error, format!("Failed to connect to {target}"));
    server.await.unwrap();
}

#[tokio::test]
async fn http_connect_trailing_bytes_rejected() {
    let target = spawn_echo().await;
    let (proxy_addr, listener) = bind_proxy().await;
    let server: JoinHandle<()> = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 4096];
        let _ = sock.read(&mut buf).await.unwrap();
        // Headers complete + unprompted trailing bytes.
        sock.write_all(b"HTTP/1.1 200 OK\r\n\r\nJUNK")
            .await
            .unwrap();
    });
    let error = error_message(
        http_proxy_connect(
            TcpStream::connect(proxy_addr).await.unwrap(),
            target,
            "",
            "",
        )
        .await,
    );
    assert_eq!(error, "Proxy has sent too many data");
    server.await.unwrap();
}
