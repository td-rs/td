//! wasm32-unknown-emscripten shim for `tdmtproto::session_connection`.
//!
//! The native session layer rides `tokio::net::TcpStream` through
//! `raw_connection`; browsers cannot open raw TCP sockets, so on wasm the
//! transport is the follow-on slice (MTProto over WebSocket, TLS inside
//! `wss://`). This shim keeps the exact public shapes the client session
//! driver typechecks against; every operation is inert and the connection
//! never becomes readable, so drivers parked on one fail fast upstream.

use tdutils::buffer::BufferSlice;
use tdutils::status::Status;

use crate::auth_data::AuthData;
use crate::message_id::MessageId;

/// `SessionConnection::Mode` — mirrors the native enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    Tcp,
    Http,
    HttpLongPoll,
}

/// `SessionConnection::Callback` — identical method set to the native trait,
/// so callback impls (the client driver's `DriverCallback`) compile unchanged.
pub trait SessionConnectionCallback {
    fn on_connected(&mut self);
    fn on_closed(&mut self, status: Status);

    fn on_server_salt_updated(&mut self);
    fn on_server_time_difference_updated(&mut self, force: bool);

    fn on_new_session_created(&mut self, unique_id: u64, first_message_id: MessageId);
    fn on_session_failed(&mut self, status: Status);

    fn on_container_sent(&mut self, container_message_id: MessageId, message_ids: Vec<MessageId>);
    fn on_pong(&mut self, ping_time: f64, pong_time: f64, current_time: f64) -> Status;

    fn on_update(&mut self, packet: BufferSlice) -> Status;

    fn on_message_ack(&mut self, message_id: MessageId);
    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        original_size: usize,
    ) -> Status;
    fn on_message_result_error(&mut self, message_id: MessageId, code: i32, message: String);
    fn on_message_failed(&mut self, message_id: MessageId, status: Status);
    fn on_message_info(
        &mut self,
        message_id: MessageId,
        state: i32,
        answer_message_id: MessageId,
        answer_size: i32,
        source: i32,
    );

    fn on_destroy_auth_key(&mut self) -> Status;
}

/// The socket stand-in: `raw_connection().stream().readable()` parks forever
/// (there is no socket to become readable on this platform yet).
pub struct ShimStream;

impl ShimStream {
    pub async fn readable(&self) -> std::io::Result<()> {
        std::future::pending().await
    }
}

/// Stands in for `raw_connection::RawConnection` on the driver-visible
/// surface (the native type itself is not linked on wasm).
pub struct RawConnection {
    stream: ShimStream,
}

impl RawConnection {
    pub fn stream(&mut self) -> &mut ShimStream {
        &mut self.stream
    }
}

/// `SessionConnection` — a value-shaped stand-in; on wasm connections are
/// never opened (`session_multi_proxy::open_connection` fails fast), so only
/// the methods the session driver typechecks against exist here.
pub struct SessionConnection {
    auth_data: AuthData,
    raw: RawConnection,
}

impl SessionConnection {
    pub fn auth_data(&mut self) -> &mut AuthData {
        &mut self.auth_data
    }

    pub fn raw_connection(&mut self) -> &mut RawConnection {
        &mut self.raw
    }

    pub fn send_query(
        &mut self,
        _buffer: BufferSlice,
        _gzip_flag: bool,
        message_id: MessageId,
        _invoke_after_message_ids: Vec<MessageId>,
        _use_quick_ack: bool,
    ) -> MessageId {
        message_id
    }

    pub fn set_online(&mut self, _online_flag: bool, _is_main: bool) {}

    pub async fn flush(&mut self, _callback: &mut dyn SessionConnectionCallback) -> f64 {
        0.0
    }

    pub fn force_close(&mut self, _callback: &mut dyn SessionConnectionCallback) {}
}
