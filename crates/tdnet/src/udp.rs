//! `td::BufferedUdp` port (`td/utils/BufferedUdp.h`) on tokio.
//!
//! Same shape as C++: datagrams are queued in both directions; a receive pops
//! one message and refills the queue with a batched read when it runs dry;
//! [`BufferedUdp::send`] only queues and [`BufferedUdp::flush_send`] drains.
//!
//! Divergences from C++, all forced by the tokio port:
//! - `tokio::net::UdpSocket` replaces `UdpSocketFd` + poll loop, so the
//!   non-blocking `receive()` becomes an async [`BufferedUdp::receive`] that
//!   awaits the socket when the queue is empty, and `sync_with_poll()` has no
//!   counterpart (readiness belongs to the runtime). tokio's try-APIs report
//!   `WouldBlock` both for real backpressure and before readiness has been
//!   observed, so a purely synchronous flush cannot distinguish them.
//! - `recvmmsg`/`sendmmsg` batching is replaced by one blocking read plus a
//!   non-blocking drain (Rust std/tokio expose no `recvmmsg`); every received
//!   datagram is copied into its own [`BufferSlice`] instead of being carved
//!   out of the C++ 16-slot shared pool.
//! - `flush_send()` drains the whole queue, awaiting writability when the
//!   kernel buffer fills; C++ stops and lets the poll loop retry.
//! - `IPAddress` is not ported; peer addresses are `std::net::SocketAddr`. A
//!   C++ error-only `UdpMessage` (empty `IPAddress`) becomes `address = None`.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;

use tokio::net::UdpSocket;

use tdutils::buffer::BufferSlice;
use tdutils::status::{Result, Status};

/// C++ `detail::UdpReaderHelper::MAX_PACKET_SIZE`: datagrams larger than this
/// are truncated, exactly as the C++ reader pool truncates them.
pub const MAX_PACKET_SIZE: usize = 2048;

/// C++ `UdpReader::BUFFER_SIZE` / `UdpWriter` message-array size: how many
/// datagrams one receive exchanges with the kernel at most.
const MAX_BATCH: usize = 16;

// Linux errno values (all Linux targets agree); C++ uses the errno macros.
const ENOMEM: i32 = 12;
const ENOBUFS: i32 = 105;

fn status_from_io(error: &std::io::Error) -> Status {
    match error.raw_os_error() {
        Some(code) => Status::posix_error(code, "UDP socket error"),
        None => Status::error_message(&format!("UDP socket error: {error}")),
    }
}

/// C++ `UdpSocketFd::is_critical_read_error`: only kernel out-of-memory
/// conditions are fatal to the socket; transport errors (e.g. ICMP
/// port-unreachable) surface as an error [`UdpMessage`] instead.
pub fn is_critical_read_error(status: &Status) -> bool {
    status.code() == ENOMEM || status.code() == ENOBUFS
}

/// C++ `td::UdpMessage`: one datagram, or a transport error.
pub struct UdpMessage {
    /// `None` mirrors the C++ empty `IPAddress` on error-only messages.
    pub address: Option<SocketAddr>,
    pub data: BufferSlice,
    pub error: Option<Status>,
}

impl UdpMessage {
    /// A datagram from (or bound for) `address`.
    pub fn new(address: SocketAddr, data: BufferSlice) -> Self {
        Self {
            address: Some(address),
            data,
            error: None,
        }
    }

    /// C++ `UdpMessage{{}, {}, std::move(status)}`: a non-critical transport
    /// error wrapped as a message so the receive loop keeps running.
    pub fn error(status: Status) -> Self {
        debug_assert!(status.is_error());
        Self {
            address: None,
            data: BufferSlice::null(),
            error: Some(status),
        }
    }
}

impl fmt::Debug for UdpMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpMessage")
            .field("address", &self.address)
            .field("data_size", &self.data.size())
            .field("error", &self.error)
            .finish()
    }
}

/// C++ `td::BufferedUdp`: a UDP socket with queued input and output.
pub struct BufferedUdp {
    socket: UdpSocket,
    input: VecDeque<UdpMessage>,
    output: VecDeque<UdpMessage>,
    recv_buf: Vec<u8>,
}

impl BufferedUdp {
    /// C++ `BufferedUdp(UdpSocketFd)` constructor.
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            input: VecDeque::new(),
            output: VecDeque::new(),
            recv_buf: vec![0; MAX_PACKET_SIZE],
        }
    }

    /// Takes a std socket; switches it to non-blocking, as tokio requires.
    pub fn from_std(socket: std::net::UdpSocket) -> Result<Self> {
        if let Err(error) = socket.set_nonblocking(true) {
            return Result::from_status(status_from_io(&error));
        }
        match UdpSocket::from_std(socket) {
            Ok(socket) => Result::Ok(Self::new(socket)),
            Err(error) => Result::from_status(status_from_io(&error)),
        }
    }

    /// C++ `UdpSocketFd::open` for a UDP socket.
    pub async fn bind(address: SocketAddr) -> Result<Self> {
        match UdpSocket::bind(address).await {
            Ok(socket) => Result::Ok(Self::new(socket)),
            Err(error) => Result::from_status(status_from_io(&error)),
        }
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Result::from(
            self.socket
                .local_addr()
                .map_err(|error| status_from_io(&error)),
        )
    }

    /// C++ `BufferedUdp::input()`: direct access to the receive queue.
    pub fn input(&mut self) -> &mut VecDeque<UdpMessage> {
        &mut self.input
    }

    /// C++ `BufferedUdp::output()`: direct access to the send queue.
    pub fn output(&mut self) -> &mut VecDeque<UdpMessage> {
        &mut self.output
    }

    /// C++ `BufferedUdp::receive()` made async: returns one message, refilling
    /// the queue with a batched read when it runs dry; awaits the socket when
    /// nothing is buffered.
    pub async fn receive(&mut self) -> Result<UdpMessage> {
        if self.input.is_empty() {
            tdutils::try_result!(_flushed, self.flush_read_once().await);
        }
        Result::Ok(self.input.pop_front().expect("a message was just queued"))
    }

    /// C++ `BufferedUdp::send()`: queues a datagram for the next flush.
    pub fn send(&mut self, message: UdpMessage) {
        self.output.push_back(message);
    }

    /// C++ `BufferedUdp::flush_send()`: drains the queue, awaiting writability
    /// when the kernel buffer is full (C++ stops and lets the poll loop
    /// retry). A queued message without an address (never produced by
    /// [`BufferedUdp::receive`]) aborts the flush with an error, mirroring the
    /// C++ `sendto` failure.
    pub async fn flush_send(&mut self) -> Result<()> {
        while let Some(message) = self.output.front() {
            let Some(address) = message.address else {
                return Result::from_status(Status::error_message(
                    "Cannot send a UDP message without an address",
                ));
            };
            loop {
                match self.socket.try_send_to(message.data.as_slice(), address) {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if let Err(wake_error) = self.socket.writable().await {
                            return Result::from_status(status_from_io(&wake_error));
                        }
                    }
                    Err(error) => return Result::from_status(status_from_io(&error)),
                }
            }
            self.output.pop_front();
        }
        Result::Ok(())
    }

    /// C++ `detail::UdpReader::read_once`: blocks for the first datagram, then
    /// drains up to [`MAX_BATCH`] - 1 more without blocking. A non-critical
    /// error on the blocking read becomes a single queued error message.
    async fn flush_read_once(&mut self) -> Result<()> {
        match self.socket.recv_from(&mut self.recv_buf).await {
            Ok((size, address)) => {
                let data = BufferSlice::from_slice_bytes(&self.recv_buf[..size]);
                self.input.push_back(UdpMessage::new(address, data));
            }
            Err(error) => return self.absorb_read_error(error),
        }
        // After a completed read, `WouldBlock` really means drained: the
        // readable readiness was just observed and consumed.
        for _ in 1..MAX_BATCH {
            match self.socket.try_recv_from(&mut self.recv_buf) {
                Ok((size, address)) => {
                    let data = BufferSlice::from_slice_bytes(&self.recv_buf[..size]);
                    self.input.push_back(UdpMessage::new(address, data));
                }
                Err(_) => break,
            }
        }
        Result::Ok(())
    }

    /// C++ `read_once` error handling: a critical error aborts; a non-critical
    /// one becomes a single error message and reading goes on.
    fn absorb_read_error(&mut self, error: std::io::Error) -> Result<()> {
        let status = status_from_io(&error);
        if is_critical_read_error(&status) {
            return Result::from_status(status);
        }
        self.input.push_back(UdpMessage::error(status));
        Result::Ok(())
    }
}
