//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProto).
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
//! S50 — the TLS layer: [`tls::SslCtx`] + [`tls::connect`]
//! (`td/net/SslCtx`, `td/net/SslStream`).
//!
//! S51 — the fake-TLS layer: [`tls_init::connect`] (`td/mtproto/TlsInit`).
//!
//! S52 — the net-query dispatch core (`td/telegram/net/{DcId,NetQuery,
//! NetActor,NetQueryDispatcher,NetQueryDelayer}`): [`net_query`].
//!
//! S56 — the HTTP framing pieces for the MTProto HTTP transport: [`http`]
//! (`HttpQuery`/`HttpHeaderCreator`/`HttpReader` + the content decoders) —
//! the C++ `HttpTransport` drives raw HTTP over the DC socket, outside
//! S48's reqwest-based `wget`.

mod transparent;

pub mod dns;
pub mod http;
pub mod http_query;
pub mod net_query;
pub mod proxy;
pub mod tcp;
pub mod tls;
pub mod tls_init;
pub mod udp;
pub mod wget;

pub use dns::{GetHostByName, NativeResolver, Options, Resolver, idn_to_ascii};
pub use proxy::{http_proxy_connect, socks5_connect};
pub use tcp::{AcceptCallback, TcpListener, connect};
pub use udp::{BufferedUdp, UdpMessage};
