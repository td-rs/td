//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProto).
//!
//! S45 — the TCP layer: [`tcp::TcpListener`] (`td/net/TcpListener`) and
//! [`tcp::connect`] (the client half of `SocketFd::open`).
//!
//! S47 — the DNS layer: [`dns::GetHostByName`] (`td::GetHostByNameActor`) and
//! [`idn_to_ascii`] (the pure host-normalization half, in [`idn`]).
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

#[cfg(not(target_family = "wasm"))]
mod transparent;

#[cfg(not(target_family = "wasm"))]
pub mod dns;
pub mod http;
pub mod http_query;
pub mod idn;
pub mod net_query;
#[cfg(not(target_family = "wasm"))]
pub mod proxy;
#[cfg(not(target_family = "wasm"))]
pub mod tcp;
#[cfg(not(target_family = "wasm"))]
#[path = "tls.rs"]
pub mod tls;
#[cfg(target_family = "wasm")]
#[path = "wasm/tls.rs"]
pub mod tls;
#[cfg(not(target_family = "wasm"))]
pub mod tls_init;
#[cfg(not(target_family = "wasm"))]
pub mod udp;
#[cfg(not(target_family = "wasm"))]
#[path = "wget.rs"]
pub mod wget;
#[cfg(target_family = "wasm")]
#[path = "wasm/wget.rs"]
pub mod wget;

#[cfg(not(target_family = "wasm"))]
pub use dns::{GetHostByName, NativeResolver, Options, Resolver};
pub use idn::idn_to_ascii;
#[cfg(not(target_family = "wasm"))]
pub use proxy::{http_proxy_connect, socks5_connect};
#[cfg(not(target_family = "wasm"))]
pub use tcp::{AcceptCallback, TcpListener, connect};
#[cfg(not(target_family = "wasm"))]
pub use udp::{BufferedUdp, UdpMessage};
