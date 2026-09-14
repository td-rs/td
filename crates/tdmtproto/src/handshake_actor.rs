//! Port of `td/mtproto/HandshakeActor.{h,cpp}` + `HandshakeConnection.h`
//! (S60) on tokio. The C++ actor (fd subscription, flush loop, timeout,
//! promise plumbing) is an async function with the same state machine — the
//! actor runtime adds nothing over tokio here, exactly like S45's async
//! `AcceptCallback` collapse.
//!
//! The C++ callback sends the next handshake query straight into the raw
//! connection while the flush runs; the Rust callback queues the query
//! instead (a callback cannot borrow the connection it is flushing), and
//! the driver writes the queue out with a second flush — same wire order.

use std::time::Duration;

use crate::auth_key::AuthKey;
use crate::handshake::{AuthKeyHandshake, AuthKeyHandshakeCallback, AuthKeyHandshakeContext};
use crate::packet_info::PacketInfo;
use crate::raw_connection::{RawConnection, RawConnectionCallback};
use crate::transport_type::TransportType;
use tdutils::buffer::BufferSlice;
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;

/// Queues handshake queries for the driver to send.
struct SendNoCrypto<'a> {
    pending: &'a mut Vec<Vec<u8>>,
}

impl AuthKeyHandshakeCallback for SendNoCrypto<'_> {
    fn send_no_crypto(&mut self, data: &[u8]) {
        self.pending.push(data.to_vec());
    }
}

/// The C++ `HandshakeConnection`: owns the raw connection, borrows the
/// handshake state machine and its context.
struct HandshakeConnection<'a> {
    raw_connection: RawConnection,
    handshake: &'a mut AuthKeyHandshake,
    context: &'a mut dyn AuthKeyHandshakeContext,
    pending: Vec<Vec<u8>>,
}

impl RawConnectionCallback for HandshakeConnection<'_> {
    fn on_raw_packet(&mut self, packet_info: &PacketInfo, mut packet: BufferSlice) -> Status {
        if !packet_info.no_crypto_flag {
            return Status::error_message("Expected not encrypted packet");
        }

        // skip header
        if packet.size() < 12 {
            return Status::error_message("Result is too small");
        }
        packet.confirm_read(12);

        let fixed_packet_size = packet.size() & !3; // remove some padded data
        self.handshake.on_message(
            &packet.as_slice()[..fixed_packet_size],
            &mut SendNoCrypto {
                pending: &mut self.pending,
            },
            self.context,
        )
    }
}

impl HandshakeConnection<'_> {
    /// One C++ `flush()`: read the pending response, then write the queued
    /// next query.
    async fn flush_once(&mut self) -> Status {
        // The raw connection and the callback borrow disjoint parts of this
        // struct; the C++ holds raw pointers for exactly this reason, and
        // the split borrow is not expressible in safe Rust.
        let raw = &mut self.raw_connection as *mut RawConnection;
        // SAFETY: `raw` aliases only `self.raw_connection`; the callback
        // (`self`) touches the other fields, never the connection itself.
        let status = unsafe { (&mut *raw).flush(&AuthKey::default(), self).await };
        if status.is_error() {
            return status;
        }
        let pending = std::mem::take(&mut self.pending);
        for query in &pending {
            self.raw_connection.send_no_crypto(query);
        }
        if !pending.is_empty() {
            // Write the queued query out; the socket may not be writable
            // yet, so this is a flush (awaits writability).
            let status = unsafe { (&mut *raw).flush(&AuthKey::default(), self).await };
            if status.is_error() {
                return status;
            }
        }
        Status::ok()
    }
}

/// `HandshakeActor`: runs the whole auth-key generation against the given
/// socket and returns the raw connection (with the generated key inside the
/// handshake).
pub async fn run_handshake(
    stream: TcpStream,
    transport_type: TransportType,
    context: &mut dyn AuthKeyHandshakeContext,
    timeout: f64,
    handshake: &mut AuthKeyHandshake,
) -> Result<RawConnection> {
    let raw_connection = RawConnection::new(stream, transport_type, None);
    let mut connection = HandshakeConnection {
        raw_connection,
        handshake,
        context,
        pending: Vec::new(),
    };
    connection.handshake.set_timeout_in(timeout);
    let status = connection.handshake.resume(&mut SendNoCrypto {
        pending: &mut connection.pending,
    });
    if status.is_error() {
        return Result::Err(status);
    }
    let status = connection.flush_once().await;
    if status.is_error() {
        return Result::Err(status);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Result::Err(Status::error_message("Timeout expired"));
            }
            _ = connection.raw_connection.stream().readable() => {}
        }

        let status = connection.flush_once().await;
        if status.is_error() {
            if status.code() == -404 {
                tdutils::log_warning!("Clear handshake [error = {}]", status.message());
                connection.handshake.clear();
            }
            return Result::Err(status);
        }
        if connection.handshake.is_ready_for_finish() {
            connection.handshake.on_finish();
            return Result::Ok(connection.raw_connection);
        }
    }
}
