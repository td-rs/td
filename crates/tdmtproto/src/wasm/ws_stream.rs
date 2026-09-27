//! The wasm byte stream over the browser WebSocket API (transport phase 2).
//!
//! `RawConnection<S>` is generic over tokio's `AsyncRead`/`AsyncWrite`;
//! native rides `tokio::net::TcpStream`, wasm rides this. The socket is
//! created on the MAIN runtime thread (`create_on_main_thread = true`) with
//! callbacks targeting it (`EM_CALLBACK_THREAD_CONTEXT_MAIN_RUNTIME_THREAD`
//! = 0x1), so they dispatch on the main loop between the harness polls;
//! sends from pthreads are proxied to the main thread by the emscripten
//! glue automatically. Wakers are thread-safe, so the tokio driver on its
//! pthread is woken from the main-thread callbacks.

use std::collections::VecDeque;
use std::ffi::c_char;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use std::sync::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

type CVoid = *mut core::ffi::c_void;

#[repr(C)]
struct CreateAttributes {
    url: *const c_char,
    protocols: *const c_char,
    create_on_main_thread: bool,
}

#[repr(C)]
struct MessageEvent {
    socket: u32,
    data: *mut u8,
    num_bytes: u32,
    is_text: bool,
}

/// `EM_CALLBACK_THREAD_CONTEXT_MAIN_RUNTIME_THREAD`.
const MAIN_RUNTIME_THREAD: usize = 0x1;

unsafe extern "C" {
    fn emscripten_websocket_new(create_attributes: *const CreateAttributes) -> i32;
    fn emscripten_websocket_set_onopen_callback_on_thread(
        socket: i32,
        user_data: CVoid,
        callback: extern "C" fn(i32, CVoid, CVoid) -> bool,
        target_thread: usize,
    ) -> i32;
    fn emscripten_websocket_set_onmessage_callback_on_thread(
        socket: i32,
        user_data: CVoid,
        callback: extern "C" fn(i32, *const MessageEvent, CVoid) -> bool,
        target_thread: usize,
    ) -> i32;
    fn emscripten_websocket_set_onerror_callback_on_thread(
        socket: i32,
        user_data: CVoid,
        callback: extern "C" fn(i32, CVoid, CVoid) -> bool,
        target_thread: usize,
    ) -> i32;
    fn emscripten_websocket_set_onclose_callback_on_thread(
        socket: i32,
        user_data: CVoid,
        callback: extern "C" fn(i32, CVoid, CVoid) -> bool,
        target_thread: usize,
    ) -> i32;
    fn emscripten_websocket_send_binary(socket: i32, data: CVoid, length: u32) -> i32;
    fn emscripten_websocket_close(socket: i32, code: u16, reason: *const c_char) -> i32;
}

struct Shared {
    state: Mutex<State>,
    notify: Notify,
    /// The staging buffer for proxied sends (see `poll_write`): owned per
    /// socket, grown on demand, replaced only by this socket's next send —
    /// never freed under a queued proxy read.
    staging: Mutex<Vec<u8>>,
}

#[derive(Default)]
struct State {
    buf: VecDeque<u8>,
    open: bool,
    closed: bool,
}

type Registry = Mutex<Vec<(i32, Arc<Shared>)>>;

fn registry() -> &'static Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn lookup(socket: i32) -> Option<Arc<Shared>> {
    registry()
        .lock()
        .unwrap()
        .iter()
        .find(|(s, _)| *s == socket)
        .map(|(_, sh)| sh.clone())
}

extern "C" fn on_open(_t: i32, _e: CVoid, socket: CVoid) -> bool {
    if let Some(shared) = lookup(socket as i32) {
        shared.state.lock().unwrap().open = true;
        shared.notify.notify_one();
    }
    true
}

extern "C" fn on_message(_t: i32, event: *const MessageEvent, socket: CVoid) -> bool {
    let Some(shared) = lookup(socket as i32) else {
        return true;
    };
    unsafe {
        let n = (*event).num_bytes as usize;
        let data = std::slice::from_raw_parts((*event).data, n);
        shared
            .state
            .lock()
            .unwrap()
            .buf
            .extend(data.iter().copied());
    }
    shared.notify.notify_one();
    true
}

extern "C" fn on_error(_t: i32, _e: CVoid, socket: CVoid) -> bool {
    if let Some(shared) = lookup(socket as i32) {
        shared.state.lock().unwrap().closed = true;
        shared.notify.notify_one();
    }
    true
}

extern "C" fn on_close(_t: i32, _e: CVoid, socket: CVoid) -> bool {
    if let Some(shared) = lookup(socket as i32) {
        shared.state.lock().unwrap().closed = true;
        shared.notify.notify_one();
    }
    true
}

/// A byte stream over one browser WebSocket connection.
pub struct WsStream {
    socket: i32,
    shared: Arc<Shared>,
}

impl WsStream {
    /// Open a WebSocket to `url` (e.g. `wss://venus.web.telegram.org/apiws`).
    /// The socket is not yet open — writes park until the open event.
    pub fn connect(url: &str) -> io::Result<Self> {
        let url = std::ffi::CString::new(url)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in url"))?;
        let attributes = CreateAttributes {
            url: url.as_ptr(),
            protocols: std::ptr::null(),
            create_on_main_thread: true,
        };
        let socket = unsafe { emscripten_websocket_new(&attributes) };
        if socket <= 0 {
            return Err(io::Error::other(format!(
                "WebSocket creation failed with code {socket}"
            )));
        }
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            notify: Notify::new(),
            staging: Mutex::new(Vec::new()),
        });
        registry().lock().unwrap().push((socket, shared.clone()));
        // SAFETY: the socket handle is valid; the callbacks are `extern "C"`
        // fns with the ABI the C header declares.
        unsafe {
            emscripten_websocket_set_onopen_callback_on_thread(
                socket,
                socket as CVoid,
                on_open,
                MAIN_RUNTIME_THREAD,
            );
            emscripten_websocket_set_onmessage_callback_on_thread(
                socket,
                socket as CVoid,
                on_message,
                MAIN_RUNTIME_THREAD,
            );
            emscripten_websocket_set_onerror_callback_on_thread(
                socket,
                socket as CVoid,
                on_error,
                MAIN_RUNTIME_THREAD,
            );
            emscripten_websocket_set_onclose_callback_on_thread(
                socket,
                socket as CVoid,
                on_close,
                MAIN_RUNTIME_THREAD,
            );
        }
        Ok(Self { socket, shared })
    }

    /// Resolve once more bytes arrived (or the stream closed) — the select
    /// arm the session driver uses on `TcpStream::readable`.
    pub async fn readable(&self) {
        loop {
            {
                let state = self.shared.state.lock().unwrap();
                if !state.buf.is_empty() || state.closed {
                    return;
                }
            }
            self.shared.notify.notified().await;
        }
    }

    /// Wait for the open event (the handshake driver writes first, so
    /// poll_write parking suffices in practice; this is for explicit waits).
    pub async fn opened(&self) {
        loop {
            {
                let state = self.shared.state.lock().unwrap();
                if state.open || state.closed {
                    return;
                }
            }
            self.shared.notify.notified().await;
        }
    }
}

impl AsyncRead for WsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut state = self.shared.state.lock().unwrap();
            if state.buf.is_empty() {
                if state.closed {
                    return Poll::Ready(Ok(()));
                }
                // Register the waker for the next on_message/notify_waiters.
                drop(state);
                let notified = self.shared.notify.notified();
                tokio::pin!(notified);
                match notified.as_mut().poll(cx) {
                    Poll::Ready(()) => continue,
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                let n = buf.remaining().min(state.buf.len());
                let mut chunks = [0u8; 65536];
                for slot in chunks.iter_mut().take(n) {
                    *slot = state.buf.pop_front().unwrap();
                }
                buf.put_slice(&chunks[..n]);
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncWrite for WsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let state = self.shared.state.lock().unwrap();
            if state.closed {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "WebSocket closed",
                )));
            }
            if !state.open {
                drop(state);
                let notified = self.shared.notify.notified();
                tokio::pin!(notified);
                match notified.as_mut().poll(cx) {
                    Poll::Ready(()) => continue,
                    Poll::Pending => return Poll::Pending,
                }
            }
            drop(state);
            break;
        }
        // send_binary from a pthread is PROXIED to the main thread: rc=0
        // only queues the call, and the wasm-side buffer can be recycled
        // before the main thread reads it (observed as deterministic
        // corruption of the later handshake rounds). Stage the bytes in a
        // per-socket buffer that outlives the proxy delay.
        let ptr = {
            let mut staging = self.shared.staging.lock().unwrap();
            if staging.len() < buf.len() {
                staging.resize(buf.len(), 0);
            }
            staging[..buf.len()].copy_from_slice(buf);
            staging.as_ptr()
        };
        let rc = unsafe {
            emscripten_websocket_send_binary(self.socket, ptr as CVoid, buf.len() as u32)
        };
        if rc == 0 {
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Ready(Err(io::Error::other(format!(
                "WebSocket send failed with code {rc}"
            ))))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        unsafe { emscripten_websocket_close(self.socket, 1000, c"".as_ptr()) };
        Poll::Ready(Ok(()))
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        unsafe { emscripten_websocket_close(self.socket, 1000, c"".as_ptr()) };
        registry()
            .lock()
            .unwrap()
            .retain(|(s, _)| *s != self.socket);
    }
}
