//! The shared machinery of the C++ `TransparentProxy` base actor
//! (`tdnet/td/net/TransparentProxy.{h,cpp}`): the 10-second whole-exchange
//! timeout, the buffered-read/write primitives over a tokio stream, and the
//! `tear_down` "unread buffered bytes" contract. Shared by the proxy
//! handshakes ([`crate::proxy`]) and the fake-TLS hello
//! ([`crate::tls_init`]).
//!
//! The C++ actor is a poll-driven state machine; the Rust port collapses it
//! onto async helpers with the same error strings:
//! * EOF mid-handshake surfaces as "Connection closed" (the C++
//!   `can_close_local` → "Connection closed" path); other I/O errors keep
//!   their OS message.
//! * The C++ `tear_down` check for unread buffered bytes (anything the
//!   peer sent beyond the handshake reply) becomes an explicit
//!   post-handshake probe — same contract, same "Proxy has sent too many
//!   data" error.

use std::future::Future;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use tdutils::status::{Result, Status};

/// The C++ `set_timeout_in(10)` whole-exchange timeout.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn status_from_io(error: &std::io::Error) -> Status {
    match error.raw_os_error() {
        Some(code) => Status::posix_error(code, &tdutils::status::strerror_safe(code)),
        None => Status::error_message(&format!("{error}")),
    }
}

/// One exact read, mirrored onto the C++ "wait until the reply is fully in
/// the buffer" poll points.
pub(crate) async fn read_exact_into(stream: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Status {
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

pub(crate) async fn write_all(stream: &mut TcpStream, data: &[u8]) -> Status {
    match stream.write_all(data).await {
        Ok(()) => Status::ok(),
        Err(e) => status_from_io(&e),
    }
}

/// One `read` of whatever is available; `Ok(0)` is the EOF error path.
pub(crate) async fn read_some(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    chunk: &mut [u8],
) -> Status {
    match stream.read(chunk).await {
        Ok(0) => Status::error_message("Connection closed"),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Status::ok()
        }
        Err(e) => status_from_io(&e),
    }
}

/// The C++ `tear_down` "unread buffered bytes" check: after a completed
/// handshake the peer must not have sent anything beyond the reply.
pub(crate) async fn reject_early_data(stream: &mut TcpStream) -> Status {
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
pub(crate) async fn with_timeout<T>(fut: impl Future<Output = T>) -> Result<T> {
    match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(value) => Result::Ok(value),
        Err(_) => Result::Err(Status::error_message("Connection timeout expired")),
    }
}
