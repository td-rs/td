//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProxy).
//!
//! S45 — the TCP layer: [`tcp::TcpListener`] (`td/net/TcpListener`) and
//! [`tcp::connect`] (the client half of `SocketFd::open`).
//!
//! S47 — the DNS layer: [`dns::GetHostByName`] (`td::GetHostByNameActor`) and
//! [`dns::idn_to_ascii`].
//!
//! S46 — the UDP layer: [`udp::BufferedUdp`] (`td/net/Udp.h`).

pub mod dns;
pub mod tcp;
pub mod udp;

pub use dns::{GetHostByName, NativeResolver, Options, Resolver, idn_to_ascii};
pub use tcp::{AcceptCallback, TcpListener, connect};
pub use udp::{BufferedUdp, UdpMessage};
