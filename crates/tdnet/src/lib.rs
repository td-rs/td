//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProxy).
//!
//! S45 — the TCP layer: [`tcp::TcpListener`] (`td/net/TcpListener`) and
//! [`tcp::connect`] (the client half of `SocketFd::open`).
//!
//! S47 — the DNS layer: [`dns::GetHostByName`] (`td::GetHostByNameActor`) and
//! [`dns::idn_to_ascii`].
//!
//! S46 — the UDP layer: [`udp::BufferedUdp`] (`td/net/Udp.h`).
//!
//! S48 — the HTTP layer: [`wget::wget`] (`td/net/Wget.h`) and [`http_query`].
//!
//! S49 — the proxy layer: [`proxy::socks5_connect`] and
//! [`proxy::http_proxy_connect`] (`td/net/Socks5`, `td/net/HttpProxy`).
//!
//! S51 — the fake-TLS layer: [`tls_init::connect`] (`td/mtproto/TlsInit`).

mod transparent;

pub mod dns;
pub mod http_query;
pub mod proxy;
pub mod tcp;
pub mod tls_init;
pub mod udp;
pub mod wget;

pub use dns::{GetHostByName, NativeResolver, Options, Resolver, idn_to_ascii};
pub use proxy::{http_proxy_connect, socks5_connect};
pub use tcp::{AcceptCallback, TcpListener, connect};
pub use udp::{BufferedUdp, UdpMessage};
