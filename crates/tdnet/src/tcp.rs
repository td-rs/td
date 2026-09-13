//! Port of `tdnet/td/net/TcpListener.{h,cpp}` on tokio, plus the client
//! connect half of `td/utils/port/SocketFd` (`SocketFd::open(address, port)`).
//!
//! Divergences from C++ (the plan maps tdnet to tokio, so the poll-driven
//! `ServerSocketFd`/`SocketFd` pair collapses onto `tokio::net`):
//!
//! - C++ dispatches accepted sockets with
//!   `send_closure(callback_, &Callback::accept, fd)`; the `send_closure`
//!   message machinery is S33, so [`AcceptCallback`] is an ordinary async
//!   trait invoked inline from the accept loop. Handlers that outlive the
//!   call must spawn (as the echo tests do) — exactly what the C++
//!   callback-as-actor gave for free.
//! - The actor lifecycle becomes plain futures: `start_up`/`loop` with the
//!   5-second bind retry is [`TcpListener::serve_with_backoff`], and the
//!   C++ `hangup()` → `stop()` teardown is dropping/aborting the future.
//! - Accepted sockets are [`TcpStream`]s (re-exported here, standing in for
//!   the C++ `SocketFd`); socket options stay a call-site decision, as in
//!   td/telegram's `ConnectionCreator` (`set_tcp_nodelay`).

use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::time::Duration;

use tokio::net::TcpListener as TokioListener;

pub use tokio::net::TcpStream;

/// The C++ retry delay after a failed `ServerSocketFd::open` —
/// `LOG(ERROR)` + `set_timeout_in(5)` before `loop()` retries `start_up()`.
pub const BIND_RETRY_DELAY: Duration = Duration::from_secs(5);

/// `SocketFd::open(address, port)` — the outgoing connect half.
pub async fn connect(address: &str, port: i32) -> io::Result<TcpStream> {
    let port = u16::try_from(port).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("port {port} out of range"),
        )
    })?;
    TcpStream::connect((address, port)).await
}

/// The accepted-socket receiver — the C++ `TcpListener::Callback` actor.
pub trait AcceptCallback: Send + 'static {
    fn accept(&mut self, stream: TcpStream) -> impl Future<Output = ()> + Send;
}

/// `td::TcpListener` — a bound server socket handing accepted streams to a
/// [`AcceptCallback`].
pub struct TcpListener {
    inner: TokioListener,
}

impl TcpListener {
    /// `ServerSocketFd::open(address, port)` — one bind attempt. Port 0 binds
    /// an ephemeral port; [`TcpListener::local_addr`] reports the result.
    /// The platform sets `SO_REUSEADDR`, as the C++ `ServerSocketFd` does.
    pub fn open(address: &str, port: i32) -> io::Result<Self> {
        let port = u16::try_from(port).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("port {port} out of range"),
            )
        })?;
        let socket = StdTcpListener::bind((address, port))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            inner: TokioListener::from_std(socket)?,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// The `TcpListener::loop` accept half — one accepted stream. Errors
    /// surface to the caller; [`TcpListener::serve`] logs and continues.
    pub async fn accept(&self) -> io::Result<TcpStream> {
        self.inner.accept().await.map(|(stream, _)| stream)
    }

    /// The `TcpListener::loop` dispatch: hand every accepted stream to
    /// `callback`, log-and-continue on accept errors (the C++ loop skips
    /// transient accept failures and only `stop()`s on hangup), and run
    /// until the future is dropped or aborted.
    pub async fn serve(self, mut callback: impl AcceptCallback) -> ! {
        loop {
            match self.accept().await {
                Ok(stream) => callback.accept(stream).await,
                Err(error) => tdutils::log_error!("{error}"),
            }
        }
    }

    /// `start_up` + `loop` with the bind retry: attempt
    /// [`TcpListener::open`], log and sleep `delay` on failure (C++ default:
    /// [`BIND_RETRY_DELAY`]) until the port is available, then serve forever.
    pub async fn serve_with_backoff(
        address: &str,
        port: i32,
        delay: Duration,
        callback: impl AcceptCallback,
    ) -> ! {
        let listener = loop {
            match Self::open(address, port) {
                Ok(listener) => break listener,
                Err(error) => {
                    tdutils::log_error!("Can't open server socket: {error}");
                    tdutils::time::sleep(delay).await;
                }
            }
        };
        listener.serve(callback).await
    }
}
