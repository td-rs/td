//! S65 proxy end-to-end: the full MTProto auth-key handshake against the
//! REAL Telegram test DC, tunneled through both simple proxies — a local
//! SOCKS5 relay (RFC 1928, no-auth + username/password) and an HTTP CONNECT
//! relay — using tdnet's ported proxy clients (S49). The live MTProxy leg
//! is covered hermetically by the S51 fake-MTProxy handshake tests
//! (crates/tdnet/tests/tls_init.rs); no public test MTProxy exists.
//!
//! Usage: proxy_e2e

use std::time::Duration;

use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdnet::proxy::{http_proxy_connect, socks5_connect};
use tdutils::status::{Result, Status};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The tdlib test-DC key (`PublicRsaKeySharedMain.cpp`).
const TEST_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// The real test DC 2 endpoint the relays forward to.
const DC_ADDR: &str = "149.154.167.40:443";

struct RsaInterface {
    rsa: Rsa,
}

impl PublicRsaKeyInterface for RsaInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        let fingerprint = self.rsa.get_fingerprint();
        if fingerprints.contains(&fingerprint) {
            Result::Ok(RsaKey {
                rsa: self.rsa.clone(),
                fingerprint,
            })
        } else {
            Result::Err(Status::error_message("No matching RSA key"))
        }
    }

    fn drop_keys(&mut self) {}
}

struct Context {
    rsa: RsaInterface,
}

impl AuthKeyHandshakeContext for Context {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        struct Callback;
        impl DhCallback for Callback {
            fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
                1
            }
            fn add_good_prime(&self, _prime_str: &[u8]) {}
            fn add_bad_prime(&self, _prime_str: &[u8]) {}
        }
        static CALLBACK: Callback = Callback;
        &CALLBACK
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

/// Handshakes against the live test DC over the given stream.
async fn handshake_over(label: &str, stream: TcpStream) {
    let rsa = Rsa::from_pem_public_key(TEST_DC_KEY).unwrap();
    let mut context = Context {
        rsa: RsaInterface { rsa },
    };
    let mut handshake = AuthKeyHandshake::new(10002, 0);
    let transport_type = TransportType::new(TransportTypeEnum::Tcp, 10002, ProxySecret::default());
    match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
        Result::Ok(_) => {
            println!(
                "proxy:{label}:handshake:ok key={:016x}",
                handshake.get_auth_key().id()
            );
        }
        Result::Err(status) => {
            println!("proxy:{label}:handshake:error:{}", status.message());
            std::process::exit(1);
        }
    }
}

/// A minimal SOCKS5 relay modeled exactly on the S49 test relay (exact-N
/// reads over one accepted connection): greeting (chosen auth), CONNECT,
/// then bidirectional pipe to the real DC. `with_auth` requires the
/// username/password method (0x02) and validates fixed credentials.
async fn socks5_relay_once(listener: TcpListener, upstream: &'static str, with_auth: bool) {
    let Ok((mut client, _)) = listener.accept().await else {
        return;
    };
    eprintln!("relay: connection accepted");
    // Greeting: [ver][nmethods][methods...]
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await.unwrap();
    assert_eq!(head[0], 5, "socks version");
    let n = head[1] as usize;
    let mut methods = vec![0u8; n];
    client.read_exact(&mut methods).await.unwrap();
    eprintln!("relay: greeting methods={methods:?}");
    let reply_method: u8 = if with_auth {
        if methods.contains(&2) { 2 } else { 0xFF }
    } else if methods.contains(&0) {
        0
    } else {
        0xFF
    };
    client.write_all(&[5, reply_method]).await.unwrap();
    if reply_method == 0xFF {
        return;
    }
    if with_auth {
        // [ver][ulen][user][plen][password] — one combined read, exactly
        // like the S49 test relay.
        eprintln!("relay: reading credentials (14 bytes)");
        let mut auth = vec![0u8; 14];
        client.read_exact(&mut auth).await.unwrap();
        eprintln!(
            "relay: credentials read: {:?}",
            String::from_utf8_lossy(&auth)
        );
        let ok =
            auth[1] == 5 && &auth[2..7] == b"td-rs" && auth[7] == 6 && &auth[8..14] == b"secret";
        client
            .write_all(&[1, if ok { 0 } else { 1 }])
            .await
            .unwrap();
        eprintln!("relay: credentials replied ok={ok}");
    }
    // CONNECT: [ver][cmd][rsv][atyp][addr][port]
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await.unwrap();
    assert_eq!(req[1], 1, "only CONNECT");
    let target = match req[3] {
        1 => {
            let mut addr = [0u8; 4];
            client.read_exact(&mut addr).await.unwrap();
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await.unwrap();
            format!(
                "{}:{}",
                std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]),
                u16::from_be_bytes(port)
            )
        }
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await.unwrap();
            let mut host = vec![0u8; len[0] as usize];
            client.read_exact(&mut host).await.unwrap();
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await.unwrap();
            format!(
                "{}:{}",
                String::from_utf8(host).unwrap(),
                u16::from_be_bytes(port)
            )
        }
        other => panic!("atyp {other}"),
    };
    assert_eq!(target, upstream, "relay target");
    eprintln!("relay: CONNECT {target}");
    let mut server = TcpStream::connect(&target).await.unwrap();
    // Reply: success, IPv4 bind 0.0.0.0:0
    client
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
}

/// A minimal HTTP CONNECT relay: reads the CONNECT request, dials the
/// target, answers 200, pipes.
async fn http_connect_relay(listener: TcpListener, upstream: &str) {
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            return;
        };
        let upstream = upstream.to_string();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            // Read until the end of the request headers.
            loop {
                if client.read_exact(&mut byte).await.is_err() {
                    return;
                }
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 8192 {
                    return;
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let target = head.split(' ').nth(1).unwrap_or_default().to_string();
            assert_eq!(target, upstream, "relay target");
            let mut server = TcpStream::connect(&target).await.unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
        });
    }
}

async fn local_listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dc_addr: std::net::SocketAddr = DC_ADDR.parse().unwrap();
    // --- SOCKS5, no auth ---
    let (listener, port) = local_listener().await;
    tokio::spawn(socks5_relay_once(listener, DC_ADDR, false));
    let relay = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let stream = socks5_connect(relay, dc_addr, "", "").await.unwrap();
    handshake_over("socks5-noauth", stream).await;

    // --- SOCKS5, username/password ---
    let (listener, port) = local_listener().await;
    tokio::spawn(socks5_relay_once(listener, DC_ADDR, true));
    let relay = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let stream = socks5_connect(relay, dc_addr, "td-rs", "secret")
        .await
        .unwrap();
    handshake_over("socks5-auth", stream).await;

    // --- HTTP CONNECT ---
    let (listener, port) = local_listener().await;
    tokio::spawn(http_connect_relay(listener, DC_ADDR));
    let relay = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let stream = http_proxy_connect(relay, dc_addr, "", "").await.unwrap();
    handshake_over("http-connect", stream).await;

    println!("S65 proxy e2e: ALL GREEN");
}
