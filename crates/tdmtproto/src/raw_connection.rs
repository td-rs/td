//! Port of `td/mtproto/RawConnection.{h,cpp}` (S62) on tokio: the socket +
//! transport pair. The C++ poll-driven `flush` loop becomes an async
//! read/write driver; the per-packet state machines stay synchronous. The
//! C++ `PollableFd` readiness subscription is replaced by awaiting socket
//! readiness between flushes (the same collapse as S45's async
//! `AcceptCallback`).

use std::collections::HashMap;

use crate::auth_key::AuthKey;
use crate::packet::{self, ReadResult};
use crate::packet_info::PacketInfo;
use crate::transport::{IStreamTransport, create_transport};
use crate::transport_type::TransportType;
use tdutils::buffer::{BufferSlice, ChainBufferWriter};
use tdutils::random;
use tdutils::status::{Result, Status};
use tokio::io::AsyncWriteExt;

/// `td::mtproto::RawConnection::StatsCallback`.
pub trait StatsCallback {
    fn on_read(&mut self, bytes: u64);
    fn on_write(&mut self, bytes: u64);
    /// Called when we know that the connection is alive.
    fn on_pong(&mut self);
    /// Called on RawConnection error. Such error should be very rare on
    /// good connections.
    fn on_error(&mut self);
    fn on_mtproto_error(&mut self);
}

/// `td::mtproto::RawConnection::PublicFields`.
#[derive(Debug, Default)]
pub struct PublicFields {
    pub extra: u32,
    pub debug_str: String,
    pub rtt: f64,
}

/// `td::mtproto::RawConnection::Callback`.
pub trait RawConnectionCallback {
    fn on_raw_packet(&mut self, packet_info: &PacketInfo, packet: BufferSlice) -> Status;
    fn on_quick_ack(&mut self, _quick_ack_token: u64) -> Status {
        Status::error_message("Quick acknowledgements aren't supported by the callback")
    }
    fn before_write(&mut self) -> Status {
        Status::ok()
    }
    fn on_read(&mut self, _size: usize) {}
}

/// `td::mtproto::RawConnectionDefault` (the non-watchOS path).
pub struct RawConnection {
    stream: tokio::net::TcpStream,
    transport: Box<dyn IStreamTransport>,
    input_writer: ChainBufferWriter,
    output_writer: ChainBufferWriter,
    quick_ack_to_token: HashMap<u32, u64>,
    has_error: bool,
    extra: PublicFields,
    stats_callback: Option<Box<dyn StatsCallback>>,
}

/// `RawConnection::MAX_PACKET_SIZE` (flush_read).
const MAX_PACKET_SIZE: usize = (1 << 22) + 1024;

impl RawConnection {
    /// `RawConnection::create`: the tokio stream takes the C++ `SocketFd`
    /// role; the transport is created from the type and takes ownership of
    /// the input reader (the C++ `input_` pointer).
    pub fn new(
        stream: tokio::net::TcpStream,
        transport_type: TransportType,
        stats_callback: Option<Box<dyn StatsCallback>>,
    ) -> Self {
        let mut transport = create_transport(transport_type);
        let mut input_writer = ChainBufferWriter::new();
        let mut output_writer = ChainBufferWriter::new();
        transport.init(input_writer.extract_reader(), &mut output_writer);
        Self {
            stream,
            transport,
            input_writer,
            output_writer,
            quick_ack_to_token: HashMap::new(),
            has_error: false,
            extra: PublicFields::default(),
            stats_callback,
        }
    }

    pub fn can_send(&self) -> bool {
        self.transport.can_write()
    }

    pub fn get_transport_type(&self) -> TransportType {
        self.transport.get_type()
    }

    pub fn stats_callback<'a>(&'a mut self) -> Option<&'a mut (dyn StatsCallback + 'static)> {
        self.stats_callback.as_deref_mut()
    }

    /// `RawConnection::send_crypto`: builds the v2 encrypted packet.
    pub fn send_crypto(
        &mut self,
        data: &[u8],
        session_id: u64,
        salt: i64,
        auth_key: &AuthKey,
        quick_ack_token: u64,
    ) -> usize {
        let mut packet_info = PacketInfo::new();
        packet_info.version = 2;
        packet_info.no_crypto_flag = false;
        packet_info.salt = salt as u64;
        packet_info.session_id = session_id;
        packet_info.use_random_padding = self.transport.use_random_padding();
        let packet = packet::write(
            data,
            auth_key,
            &mut packet_info,
            self.transport.max_prepend_size(),
            self.transport.max_append_size(),
        );

        let mut use_quick_ack = false;
        if quick_ack_token != 0 && self.transport.support_quick_ack() {
            assert!(packet_info.message_ack & (1 << 31) != 0);
            let message_ack = packet_info.message_ack;
            let token = self.quick_ack_to_token.entry(message_ack).or_insert(quick_ack_token);
            if *token == quick_ack_token {
                use_quick_ack = true;
            } else {
                tdutils::log_error!("Quick ack {message_ack} collision");
            }
        }

        let packet_size = packet.size();
        self.transport.write(packet, use_quick_ack, &mut self.output_writer);
        packet_size
    }

    /// `RawConnection::send_no_crypto` — the C++ wraps the query in
    /// `PacketStorer<NoCryptoImpl>(MessageId(), storer)` at the
    /// `HandshakeConnection` call site: msg_id (0 for handshakes) +
    /// inner length + body + random padding, on top of the transport's
    /// auth_key_id-only no-crypto header.
    pub fn send_no_crypto(&mut self, data: &[u8]) {
        let mut pad_size = (!data.len() + 1) & 15; // (-data.len()) & 15
        pad_size += 16 * (random::secure_uint32() as usize % 16);
        let mut wrapped = Vec::with_capacity(12 + data.len() + pad_size);
        wrapped.extend_from_slice(&0u64.to_le_bytes()); // MessageId()
        wrapped.extend_from_slice(&((data.len() + pad_size) as u32).to_le_bytes());
        wrapped.extend_from_slice(data);
        wrapped.resize(12 + data.len() + pad_size, 0);
        random::secure_bytes(&mut wrapped[12 + data.len()..]);

        let mut packet_info = PacketInfo::new();
        packet_info.no_crypto_flag = true;
        let packet = packet::write(
            &wrapped,
            &AuthKey::default(),
            &mut packet_info,
            self.transport.max_prepend_size(),
            self.transport.max_append_size(),
        );

        tdutils::log_info!("Send handshake packet of size {}", packet.size());
        self.transport.write(packet, false, &mut self.output_writer);
    }

    pub fn stream(&mut self) -> &mut tokio::net::TcpStream {
        &mut self.stream
    }

    pub fn extra(&self) -> &PublicFields {
        &self.extra
    }

    pub fn extra_mut(&mut self) -> &mut PublicFields {
        &mut self.extra
    }

    pub fn has_error(&self) -> bool {
        self.has_error
    }

    pub fn close(&mut self) {
        // The C++ resets the transport and closes the socket; the Rust
        // stream closes when the connection is dropped. Nothing further is
        // sent after close().
        tdutils::log_debug!("Close raw connection");
        self.has_error = true;
    }

    /// The async `RawConnection::flush`: read everything available, process
    /// packets, let the callback queue writes, then drain the output.
    pub async fn flush(
        &mut self,
        auth_key: &AuthKey,
        callback: &mut dyn RawConnectionCallback,
    ) -> Status {
        if self.has_error {
            return Status::error_message("Connection has already failed");
        }

        let status = self.flush_read(auth_key, callback);
        if status.is_error() {
            return self.fail(status);
        }
        let status = callback.before_write();
        if status.is_error() {
            return self.fail(status);
        }
        let status = self.flush_write().await;
        if status.is_error() {
            return self.fail(status);
        }
        Status::ok()
    }

    fn fail(&mut self, status: Status) -> Status {
        if let Some(stats) = self.stats_callback.as_deref_mut() {
            if status.code() != 2 {
                stats.on_error();
            }
        }
        self.has_error = true;
        status
    }

    /// The synchronous read half: try_read the socket, then drain complete
    /// transport frames into the callback. The stats callback is taken out
    /// of `self` for the loop so the packet handlers can borrow `self`
    /// mutably without aliasing it.
    fn flush_read(
        &mut self,
        auth_key: &AuthKey,
        callback: &mut dyn RawConnectionCallback,
    ) -> Status {
        // Non-blocking socket read into the transport's input stream.
        let mut buf = [0u8; 65536];
        match self.stream.try_read(&mut buf) {
            Ok(0) => return Status::error(2, "Connection closed"),
            Ok(size) => {
                self.input_writer.append(&buf[..size], 0);
                if size != 0 {
                    if let Some(stats) = self.stats_callback.as_deref_mut() {
                        stats.on_read(size as u64);
                    }
                    callback.on_read(size);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Status::error(2, &error.to_string()),
        }

        while self.transport.can_read() {
            let mut packet = BufferSlice::null();
            let mut quick_ack = 0;
            let mut error_code = 0;
            let wait_size = match self.transport.read_next(
                &mut packet,
                &mut quick_ack,
                &mut error_code,
            ) {
                Result::Ok(size) => size,
                Result::Err(status) => return status,
            };
            if wait_size != 0 {
                if wait_size > MAX_PACKET_SIZE {
                    return Status::error_message(&format!(
                        "Expected packet size is too big: {wait_size}"
                    ));
                }
                break;
            }
            if quick_ack != 0 {
                let status = on_quick_ack(&mut self.quick_ack_to_token, quick_ack, callback);
                if status.is_error() {
                    return status;
                }
                continue;
            }

            let mut packet_info = PacketInfo::new();
            packet_info.version = 2;

            let read_result = match packet::read(
                packet.as_mutable_slice(),
                error_code,
                auth_key,
                &mut packet_info,
            ) {
                Result::Ok(result) => result,
                Result::Err(status) => return status,
            };
            match read_result {
                ReadResult::Quickack(qa) => {
                    let status = on_quick_ack(&mut self.quick_ack_to_token, qa, callback);
                    if status.is_error() {
                        return status;
                    }
                }
                ReadResult::Error(code) => {
                    let status = on_read_mtproto_error(code, &mut self.stats_callback);
                    if status.is_error() {
                        return status;
                    }
                }
                ReadResult::Packet { offset, len } => {
                    // If a packet was successfully decrypted, then it is ok
                    // to assume that the connection is alive.
                    if !auth_key.empty() {
                        if let Some(stats) = self.stats_callback.as_deref_mut() {
                            stats.on_pong();
                        }
                    }
                    let packet = packet.from_slice(&packet.as_slice()[offset..offset + len]);
                    let status = callback.on_raw_packet(&packet_info, packet);
                    if status.is_error() {
                        return status;
                    }
                }
                ReadResult::Nop => {}
            }
        }
        Status::ok()
    }

    /// The async write half: drain the output buffer into the socket. The
    /// writer is recreated afterwards (`extract_reader` moves its head out;
    /// the transports take the writer per call).
    async fn flush_write(&mut self) -> Status {
        let mut reader = self.output_writer.extract_reader();
        let mut written = 0usize;
        while !reader.empty() {
            let slice = reader.prepare_read();
            match self.stream.write(slice).await {
                Ok(size) => {
                    if size == 0 {
                        return Status::error(2, "Connection closed");
                    }
                    reader.confirm_read(size);
                    written += size;
                }
                Err(error) => return Status::error(2, &error.to_string()),
            }
        }
        self.output_writer = ChainBufferWriter::new();
        if written > 0 {
            if let Some(stats) = self.stats_callback.as_deref_mut() {
                stats.on_write(written as u64);
            }
        }
        Status::ok()
    }
}

/// `RawConnectionDefault::on_quick_ack` — a free function so the caller can
/// borrow disjoint fields of the connection.
fn on_quick_ack(
    quick_ack_to_token: &mut HashMap<u32, u64>,
    quick_ack: u32,
    callback: &mut dyn RawConnectionCallback,
) -> Status {
    if quick_ack & (1 << 31) == 0 {
        tdutils::log_error!("Receive invalid quick_ack {quick_ack}");
        return Status::ok();
    }
    let token = match quick_ack_to_token.remove(&quick_ack) {
        Some(token) => token,
        None => {
            tdutils::log_warning!("Receive unknown quick_ack {quick_ack}");
            return Status::ok();
        }
    };
    callback.on_quick_ack(token)
}

/// `RawConnectionDefault::on_read_mtproto_error`.
fn on_read_mtproto_error(error_code: i32, stats: &mut Option<Box<dyn StatsCallback>>) -> Status {
    if error_code == -429 {
        if let Some(stats) = stats.as_deref_mut() {
            stats.on_mtproto_error();
        }
        return Status::error(500, &format!("MTProto error: {error_code}"));
    }
    if error_code == -404 {
        return Status::error(-404, &format!("MTProto error: {error_code}"));
    }
    Status::error_message(&format!("MTProto error: {error_code}"))
}
