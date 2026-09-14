//! Port of `td/mtproto/PingConnection.{h,cpp}` + `td/mtproto/Ping.{h,cpp}`
//! (S63): connection pings — either unencrypted `req_pq_multi` pings (used
//! to measure raw connectivity/RTT before a handshake) or encrypted
//! ping-pongs through a full [`SessionConnection`] (used to validate a
//! persisted auth key). The C++ `PingActor` fd-loop is an async driver.

use std::time::Duration;

use crate::auth_data::AuthData;
use crate::auth_key::AuthKey;
use crate::handshake::int128_from_bytes;
use crate::message_id::MessageId;
use crate::mtproto_api;
use crate::packet_info::PacketInfo;
use crate::raw_connection::{RawConnection, RawConnectionCallback};
use crate::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use crate::transport_type::TransportType;
use tdutils::buffer::BufferSlice;
use tdutils::random;
use tdutils::status::{Result, Status};
use tdutils::time;
use tokio::net::TcpStream;

/// Counts received (unencrypted) packets — the `PingConnectionReqPQ`
/// callback.
struct PongCounter {
    count: usize,
}

impl RawConnectionCallback for PongCounter {
    fn on_raw_packet(&mut self, _packet_info: &PacketInfo, mut packet: BufferSlice) -> Status {
        if packet.size() < 12 {
            return Status::error_message("Result is too small");
        }
        packet.confirm_read(12);
        self.count += 1;
        Status::ok()
    }
}

/// The `PingConnectionPingPong` session callback: counts pongs and measures
/// the RTT between the first and the second.
struct PingPongSession {
    pong_cnt: i32,
    rtt: f64,
    is_closed: bool,
    status: Status,
}

impl PingPongSession {
    fn new() -> Self {
        Self {
            pong_cnt: 0,
            rtt: 0.0,
            is_closed: false,
            status: Status::ok(),
        }
    }

    fn was_pong(&self) -> bool {
        self.pong_cnt >= 2
    }
}

impl SessionConnectionCallback for PingPongSession {
    fn on_connected(&mut self) {}
    fn on_closed(&mut self, status: Status) {
        self.is_closed = true;
        assert!(status.is_error());
        self.status = status;
    }
    fn on_server_salt_updated(&mut self) {}
    fn on_server_time_difference_updated(&mut self, _force: bool) {}
    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}
    fn on_session_failed(&mut self, _status: Status) {}
    fn on_container_sent(&mut self, _container_message_id: MessageId, _message_ids: Vec<MessageId>) {}
    fn on_pong(&mut self, _ping_time: f64, _pong_time: f64, _current_time: f64) -> Status {
        self.pong_cnt += 1;
        if self.pong_cnt == 1 {
            self.rtt = time::now();
        } else if self.pong_cnt == 2 {
            self.rtt = time::now() - self.rtt;
        }
        Status::ok()
    }
    fn on_update(&mut self, _packet: BufferSlice) -> Status {
        Status::ok()
    }
    fn on_message_ack(&mut self, _message_id: MessageId) {}
    fn on_message_result_ok(
        &mut self,
        _message_id: MessageId,
        _packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        tdutils::log_error!("Unexpected message");
        Status::ok()
    }
    fn on_message_result_error(&mut self, _message_id: MessageId, _code: i32, _message: String) {}
    fn on_message_failed(&mut self, _message_id: MessageId, _status: Status) {}
    fn on_message_info(
        &mut self,
        _message_id: MessageId,
        _state: i32,
        _answer_message_id: MessageId,
        _answer_size: i32,
        _source: i32,
    ) {
    }
    fn on_destroy_auth_key(&mut self) -> Status {
        tdutils::log_error!("Destroy auth key");
        Status::ok()
    }
}

/// `td::mtproto::create_ping_actor`: pings the connection (encrypted
/// ping-pong when `auth_data` is given, `req_pq` pings otherwise) and
/// returns the connection with its measured RTT in `extra().rtt`.
pub async fn run_ping(
    stream: TcpStream,
    transport_type: TransportType,
    auth_data: Option<AuthData>,
) -> Result<RawConnection> {
    let raw_connection = RawConnection::new(stream, transport_type, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(10.0);
    match auth_data {
        Some(mut auth_data) => {
            auth_data.set_header(Vec::new());
            auth_data.clear_seq_no();
            let mut connection =
                SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);
            let mut state = PingPongSession::new();
            loop {
                let wakeup_at = connection.flush(&mut state).await;
                if state.was_pong() {
                    connection.raw_connection().extra_mut().rtt = state.rtt;
                    return Result::Ok(connection.move_as_raw_connection());
                }
                if state.is_closed {
                    return Result::Err(state.status.clone());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Result::Err(Status::error_message("Pong timeout expired"));
                }
                let wait = (wakeup_at - time::now()).max(0.001);
                tokio::select! {
                    _ = connection.raw_connection().stream().readable() => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_secs_f64(wait)) => {}
                }
            }
        }
        None => {
            // req_pq pings: send one req_pq per response, measure the RTT
            // of the last round (the C++ `PingConnectionReqPQ`).
            let mut raw_connection = raw_connection;
            let auth_key = AuthKey::default();
            let mut remaining = 2usize;
            let mut ping_sent = false;
            let mut start_time = 0.0;
            loop {
                if !ping_sent {
                    if remaining == 1 {
                        start_time = time::now();
                    }
                    let mut nonce = [0u8; 16];
                    random::secure_bytes(&mut nonce);
                    let mut data = Vec::new();
                    mtproto_api::req_pq_multi {
                        nonce: int128_from_bytes(&nonce),
                    }
                    .store_boxed(&mut data);
                    raw_connection.send_no_crypto(&data);
                    ping_sent = true;
                }
                let mut counter = PongCounter { count: 0 };
                let status = raw_connection.flush(&auth_key, &mut counter).await;
                if status.is_error() {
                    return Result::Err(status);
                }
                if counter.count > 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        raw_connection.extra_mut().rtt = time::now() - start_time;
                        return Result::Ok(raw_connection);
                    }
                    ping_sent = false;
                    continue;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Result::Err(Status::error_message("Pong timeout expired"));
                }
                tokio::select! {
                    _ = raw_connection.stream().readable() => {}
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        }
    }
}
