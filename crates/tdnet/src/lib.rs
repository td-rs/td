//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProxy).
//!
//! S45 — the TCP layer: [`tcp::TcpListener`] (`td/net/TcpListener`) and
//! [`tcp::connect`] (the client half of `SocketFd::open`).

pub mod tcp;

pub use tcp::{AcceptCallback, TcpListener, connect};
