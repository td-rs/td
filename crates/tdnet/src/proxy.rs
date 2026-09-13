//! Port of `tdnet/td/net/TransparentProxy.{h,cpp}` and its two protocols,
//! `Socks5.{h,cpp}` and `HttpProxy.{h,cpp}`, on tokio.
//!
//! The C++ `TransparentProxy` is a poll-driven actor state machine over a
//! `BufferedFd<SocketFd>` with a 10-second timeout and a promise-style
//! callback; the Rust port collapses it onto two async functions with the
//! same wire protocol and the same error strings.
//!
//! Documented deviations from the C++ actor:
//! * Callback/Promise → `Result<TcpStream>`; dropping the future is the
//!   C++ `hangup()` → "Canceled" path.
//! * The C++ `tear_down` check for unread buffered bytes (anything the
//!   proxy sent beyond the handshake reply) becomes an explicit
//!   post-handshake probe — same contract, same "Proxy has sent too many
//!   data" error.
//! * EOF mid-handshake surfaces as "Connection closed" (the C++
//!   `can_close_local` → "Connection closed" path); other I/O errors keep
//!   their OS message.
//! * `Callback::on_connected` (a stats hook between authentication and the
//!   target-address request) has no Rust consumer yet and is a no-op.
//! * HTTP CONNECT keeps the C++ host formatting verbatim — `<ip>:<port>`
//!   with no IPv6 brackets — so IPv6 targets are as malformed here as they
//!   are upstream.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use tdutils::status::{Result, Status};

/// The C++ `set_timeout_in(10)` whole-exchange timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn status_from_io(error: &std::io::Error) -> Status {
    match error.raw_os_error() {
        Some(code) => Status::posix_error(code, &tdutils::status::strerror_safe(code)),
        None => Status::error_message(&format!("{error}")),
    }
}

/// One exact read, mirrored onto the C++ "wait until the reply is fully in
/// the buffer" poll points. EOF mid-reply is the C++ `can_close_local` →
/// "Connection closed" path.
async fn read_exact_into(stream: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Status {
    let start = buf.len();
    buf.resize(start + n, 0);
    match stream.read_exact(&mut buf[start..]).await {
        Ok(_) => Status::ok(),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Status::error_message("Connection closed")
        }
        Err(e) => status_from_io(&e),
    }
}

async fn write_all(stream: &mut TcpStream, data: &[u8]) -> Status {
    match stream.write_all(data).await {
        Ok(()) => Status::ok(),
        Err(e) => status_from_io(&e),
    }
}

/// The C++ `tear_down` "unread buffered bytes" check: after a completed
/// handshake the proxy must not have sent anything beyond the reply.
async fn reject_early_data(stream: &mut TcpStream) -> Status {
    let mut probe = [0u8; 16];
    match stream.try_read(&mut probe) {
        Ok(0) => Status::error_message("Connection closed"),
        Ok(_) => Status::error_message("Proxy has sent too many data"),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Status::ok(),
        Err(e) => status_from_io(&e),
    }
}

/// The C++ `set_timeout_in(10)` whole-exchange timeout, with the exact
/// `timeout_expired` error string.
async fn with_timeout(fut: impl Future<Output = Status>) -> Status {
    match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(status) => status,
        Err(_) => Status::error_message("Connection timeout expired"),
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | (chunk.get(2).copied().unwrap_or(0) as u32);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `td::Socks5` — establish a tunnel to `target` over an already-connected
/// SOCKS5 proxy stream (RFC 1928 + the RFC 1929 username/password subset
/// the C++ actor speaks).
pub async fn socks5_connect(
    mut stream: TcpStream,
    target: SocketAddr,
    username: &str,
    password: &str,
) -> Result<TcpStream> {
    let status = with_timeout(socks5_handshake(&mut stream, target, username, password)).await;
    if status.is_error() {
        return Result::from_status(status);
    }
    tdutils::try_status!(reject_early_data(&mut stream).await);
    Result::Ok(stream)
}

async fn socks5_handshake(
    stream: &mut TcpStream,
    target: SocketAddr,
    username: &str,
    password: &str,
) -> Status {
    // `Socks5::send_greeting`.
    let use_username = !username.is_empty();
    let mut greeting = vec![0x05, if use_username { 2 } else { 1 }, 0x00];
    if use_username {
        greeting.push(0x02);
    }
    tdutils::try_status!(write_all(stream, &greeting).await);

    // `Socks5::wait_greeting_response`.
    let mut buf = Vec::with_capacity(4 + 16 + 2);
    tdutils::try_status!(read_exact_into(stream, &mut buf, 2).await);
    if buf[0] != 0x05 {
        return Status::error_message(&format!("Unsupported socks protocol version {}", buf[0]));
    }
    match buf[1] {
        0x00 => {}
        0x02 => {
            // `Socks5::send_username_password`.
            if username.len() >= 128 {
                return Status::error_message("Username is too long");
            }
            if password.len() >= 128 {
                return Status::error_message("Password is too long");
            }
            let mut request = Vec::with_capacity(3 + username.len() + password.len());
            request.push(0x01);
            request.push(username.len() as u8);
            request.extend_from_slice(username.as_bytes());
            request.push(password.len() as u8);
            request.extend_from_slice(password.as_bytes());
            tdutils::try_status!(write_all(stream, &request).await);

            // `Socks5::wait_password_response`.
            buf.clear();
            tdutils::try_status!(read_exact_into(stream, &mut buf, 2).await);
            if buf[0] != 0x01 {
                return Status::error_message(&format!(
                    "Unsupported socks subnegotiation protocol version {}",
                    buf[0]
                ));
            }
            if buf[1] != 0x00 {
                return Status::error_message("Wrong username or password");
            }
        }
        _ => return Status::error_message("Unsupported authentication mode"),
    }

    // `Socks5::send_ip_address` (the C++ `callback_->on_connected()` stats
    // hook fires here; it has no Rust consumer yet).
    let mut request = vec![0x05, 0x01, 0x00];
    match target.ip() {
        std::net::IpAddr::V4(ip) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        std::net::IpAddr::V6(ip) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    tdutils::try_status!(write_all(stream, &request).await);

    // `Socks5::wait_ip_address_response`.
    buf.clear();
    tdutils::try_status!(read_exact_into(stream, &mut buf, 4).await);
    if buf[0] != 0x05 {
        return Status::error_message("Invalid response");
    }
    if buf[1] != 0x00 {
        return Status::error_message(&format!("Receive error code {} from server", buf[1]));
    }
    if buf[2] != 0x00 {
        return Status::error_message("Byte must be zero");
    }
    let address_len = match buf[3] {
        0x01 => 4,
        0x04 => 16,
        _ => return Status::error_message("Invalid response"),
    };
    read_exact_into(stream, &mut buf, address_len + 2).await
}

/// `td::HttpProxy` — establish a tunnel to `target` over an
/// already-connected HTTP CONNECT proxy stream.
pub async fn http_proxy_connect(
    mut stream: TcpStream,
    target: SocketAddr,
    username: &str,
    password: &str,
) -> Result<TcpStream> {
    let mut leftover = Vec::new();
    let status = with_timeout(http_handshake(
        &mut stream,
        target,
        username,
        password,
        &mut leftover,
    ))
    .await;
    if status.is_error() {
        return Result::from_status(status);
    }
    // The C++ `tear_down` check: `advance(total_size)` leaves any reply
    // trailing bytes unread, and an unread byte is an error.
    if !leftover.is_empty() {
        return Result::Err(Status::error_message("Proxy has sent too many data"));
    }
    tdutils::try_status!(reject_early_data(&mut stream).await);
    Result::Ok(stream)
}

async fn http_handshake(
    stream: &mut TcpStream,
    target: SocketAddr,
    username: &str,
    password: &str,
    leftover: &mut Vec<u8>,
) -> Status {
    // `HttpProxy::send_connect` — verbatim host formatting.
    let host = format!("{}:{}", target.ip(), target.port());
    let mut request = format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n");
    if !username.is_empty() || !password.is_empty() {
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64_encode(format!("{username}:{password}").as_bytes())
        ));
    }
    request.push_str("\r\n");
    tdutils::try_status!(write_all(stream, request.as_bytes()).await);

    // `HttpProxy::wait_connect_response`, incremental: the C++ actor
    // re-runs the parser on every poll with whatever the buffer holds, so
    // every wait point is an explicit retry on more input.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        // The C++ gate: `it.size() < 12 + 1 + 1`.
        if buf.len() < 14 {
            tdutils::try_status!(read_some(stream, &mut buf, &mut chunk).await);
            continue;
        }
        if (!buf.starts_with(b"HTTP/1.1 2") && !buf.starts_with(b"HTTP/1.0 2"))
            || !buf[10].is_ascii_digit()
            || !buf[11].is_ascii_digit()
        {
            return Status::error_message(&format!("Failed to connect to {host}"));
        }
        // End of the status line.
        let Some(status_end) = buf[12..].iter().position(|&b| b == b'\n') else {
            tdutils::try_status!(read_some(stream, &mut buf, &mut chunk).await);
            continue;
        };
        // Scan for the empty line, counting bytes like the C++ actor:
        // a lineless '\n' (pos == 0) or a lone '\r' before it.
        let mut pos = 0usize;
        let mut prev = b'\n';
        let mut terminator = None;
        for (offset, &c) in buf[12 + status_end + 1..].iter().enumerate() {
            if c == b'\n' {
                if pos == 0 || (pos == 1 && prev == b'\r') {
                    terminator = Some(12 + status_end + 1 + offset);
                    break;
                }
                pos = 0;
            } else {
                pos += 1;
            }
            prev = c;
        }
        let Some(terminator) = terminator else {
            tdutils::try_status!(read_some(stream, &mut buf, &mut chunk).await);
            continue;
        };
        leftover.extend_from_slice(&buf[terminator + 1..]);
        return Status::ok();
    }
}

async fn read_some(stream: &mut TcpStream, buf: &mut Vec<u8>, chunk: &mut [u8]) -> Status {
    match stream.read(chunk).await {
        Ok(0) => Status::error_message("Connection closed"),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Status::ok()
        }
        Err(e) => status_from_io(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_td() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
