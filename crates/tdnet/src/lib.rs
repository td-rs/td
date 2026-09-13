//! Port of `td/net` on tokio: TCP/UDP, DNS, HTTP, proxies, fake-TLS (MTProxy).
//!
//! S47 — the DNS layer: [`dns::GetHostByName`] (`td::GetHostByNameActor`) and
//! [`dns::idn_to_ascii`].

pub mod dns;

pub use dns::{GetHostByName, NativeResolver, Options, Resolver, idn_to_ascii};
