//! Port of `tdnet/td/net/GetHostByNameActor.{h,cpp}` — DNS resolution with an
//! affirmative/negative result cache and in-flight query coalescing — plus the
//! `idn_to_ascii` host normalization it relies on
//! (`td/utils/port/IPAddress.cpp`).
//!
//! Divergences from C++ (tdnet maps onto tokio, and the actor machinery is
//! S33):
//! - The actor becomes [`GetHostByName`]: shared state behind an `Arc`, the
//!   per-resolver actors become one spawned task walking the resolver chain
//!   (C++ `run_query`/`on_query_result` with `Query::pos`), and the
//!   `Query::promises` list becomes per-caller `oneshot` receivers.
//! - `Options::scheduler_id` is dropped (no scheduler until S33); the
//!   blocking `getaddrinfo` runs on the tokio blocking pool via
//!   `spawn_blocking`, which is what the C++ `NativeDnsResolver` actor on its
//!   own scheduler achieved.
//! - `ResolverType::Google` (the `https://dns.google/resolve` fallback) needs
//!   the HTTP client and TLS (S48/S50); until then `Options::resolvers`
//!   holds the ordered chain and only [`NativeResolver`] exists. The default
//!   chain matches the C++ `resolver_types{ResolverType::Native}`.
//! - A panicking resolver would strand the C++ actor's promise list; each hop
//!   runs as its own task, so the panic surfaces as an error result and the
//!   cache and waiters are still flushed.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::idn_to_ascii;
use tdutils::status::Status;
use tdutils::time;

use tokio::sync::oneshot;

/// `GetHostByNameActor::Options::DEFAULT_CACHE_TIME` — 29 minutes.
pub const DEFAULT_CACHE_TIME: Duration = Duration::from_secs(60 * 29);

/// `GetHostByNameActor::Options::DEFAULT_ERROR_CACHE_TIME` — 5 minutes.
pub const DEFAULT_ERROR_CACHE_TIME: Duration = Duration::from_secs(60 * 5);

/// The boxed future every [`Resolver`] returns.
pub type ResolveFuture = Pin<Box<dyn Future<Output = Result<IpAddr, Status>> + Send>>;

/// One resolver step — the C++ `detail::NativeDnsResolver` /
/// `detail::GoogleDnsResolver` actors. Implementations must not resolve the
/// same host differently for the same `prefer_ipv6`, since results are cached
/// per family.
pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: &str, prefer_ipv6: bool) -> ResolveFuture;
}

/// `IPAddress::init_host_port(host, 0, prefer_ipv6)` selection loop — the
/// first address of the preferred family, else the first address of the other
/// one. Bracketed IPv6 literals never reach `getaddrinfo` (C++
/// `init_ipv6_port`), so they are parsed directly.
pub(crate) fn resolve_native(host: &str, prefer_ipv6: bool) -> Result<IpAddr, Status> {
    if host.is_empty() {
        return Err(Status::error_message("Host is empty"));
    }

    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner.parse::<Ipv6Addr>().map(IpAddr::V6).map_err(|_| {
            Status::error_message(&format!("\"{host}\" is not a valid IPv6 address"))
        });
    }

    use std::net::ToSocketAddrs;
    match (host, 0).to_socket_addrs() {
        Err(error) => Err(Status::error_message(&format!(
            "Failed to resolve host: {error}"
        ))),
        Ok(addrs) => pick_addr(addrs.map(|addr| addr.ip()), prefer_ipv6)
            .ok_or_else(|| Status::error_message("Failed to find IPv4/IPv6 address")),
    }
}

/// The C++ `best_info` walk: use the first IPv4 address unless IPv6 is (or
/// must be) preferred, and vice versa.
fn pick_addr(addrs: impl Iterator<Item = IpAddr>, prefer_ipv6: bool) -> Option<IpAddr> {
    let mut best = None;
    for ip in addrs {
        let matches_preference = ip.is_ipv6() == prefer_ipv6;
        if matches_preference {
            return Some(ip);
        }
        if best.is_none() {
            best = Some(ip);
        }
    }
    best
}

/// The C++ `detail::NativeDnsResolver` — platform `getaddrinfo` on the tokio
/// blocking pool.
pub struct NativeResolver;

impl Resolver for NativeResolver {
    fn resolve(&self, host: &str, prefer_ipv6: bool) -> ResolveFuture {
        let host = host.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || resolve_native(&host, prefer_ipv6))
                .await
                .unwrap_or_else(|error| {
                    Err(Status::error_message(&format!(
                        "DNS resolver task failed: {error}"
                    )))
                })
        })
    }
}

/// `GetHostByNameActor::Options` minus `scheduler_id` (see the module docs).
#[derive(Clone)]
pub struct Options {
    /// The C++ `resolver_types` chain, tried in order until one succeeds.
    pub resolvers: Vec<Arc<dyn Resolver>>,
    /// How long a successful result stays cached (`ok_timeout`).
    pub ok_timeout: Duration,
    /// How long a failed lookup stays cached (`error_timeout`).
    pub error_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            resolvers: vec![Arc::new(NativeResolver)],
            ok_timeout: DEFAULT_CACHE_TIME,
            error_timeout: DEFAULT_ERROR_CACHE_TIME,
        }
    }
}

/// The cached `Result<IPAddress>` + expiry — C++ `GetHostByNameActor::Value`.
struct Value {
    ip: Result<IpAddr, Status>,
    /// On the `tdutils::time::now()` clock, like every C++ `expires_at`.
    expires_at: f64,
}

impl Value {
    /// `Value::get_ip_port` — the port is stamped at delivery; the cache
    /// stays port-free so one entry serves every port.
    fn get_ip_port(&self, port: u16) -> Result<SocketAddr, Status> {
        self.ip.clone().map(|ip| SocketAddr::new(ip, port))
    }
}

/// One in-flight query's waiter list — the C++ `Query::promises`.
type Waiters = Vec<oneshot::Sender<Result<IpAddr, Status>>>;

/// Per-family state (`cache_[prefer_ipv6]` / `active_queries_[prefer_ipv6]`).
struct State {
    cache: [HashMap<String, Value>; 2],
    active: [HashMap<String, Waiters>; 2],
}

struct Inner {
    state: Mutex<State>,
    options: Options,
}

impl Inner {
    /// The resolver chain for one (host, family): C++ `run_query` creates the
    /// `query.pos`-th resolver actor and `on_query_result` advances `pos`
    /// while results are errors; here the loop runs inside the task the first
    /// caller spawned. The final result — success or error — is cached with
    /// `ok_timeout`/`error_timeout` and delivered to every coalesced caller.
    async fn resolve_query(&self, host: String, prefer_ipv6: bool) {
        let mut result = Err(Status::error_message("Host is empty"));
        for resolver in &self.options.resolvers {
            // Each hop runs as its own task (the C++ per-resolver actor), so
            // a panicking resolver surfaces as a JoinError instead of
            // stranding the waiters.
            result = match tokio::spawn(resolver.resolve(&host, prefer_ipv6)).await {
                Ok(result) => result,
                Err(join_error) if join_error.is_panic() => {
                    let panic = join_error.into_panic();
                    Err(Status::error_message(&format!(
                        "DNS resolver panicked: {}",
                        panic_message(panic)
                    )))
                }
                Err(join_error) => Err(Status::error_message(&format!(
                    "DNS resolver task failed: {join_error}"
                ))),
            };
            if result.is_ok() {
                break;
            }
        }

        let timeout = if result.is_ok() {
            self.options.ok_timeout
        } else {
            self.options.error_timeout
        };
        let expires_at = time::now() + timeout.as_secs_f64();

        let waiters = {
            let mut state = self.state.lock();
            state.cache[usize::from(prefer_ipv6)].insert(
                host.clone(),
                Value {
                    ip: result.clone(),
                    expires_at,
                },
            );
            state.active[usize::from(prefer_ipv6)]
                .remove(&host)
                .expect("completed DNS query was registered")
        };
        for sender in waiters {
            let _ = sender.send(result.clone());
        }
    }
}

/// Extract a printable message from a panic payload. Takes the payload by
/// value: `&Box<dyn Any>` coerces to `&dyn Any` *of the box itself* (the Box
/// is `Sized + Any`), which silently breaks every `downcast_ref`.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    let panic = &*panic;
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// `td::GetHostByNameActor` — a shared DNS resolver service. Cheap to clone;
/// every clone sees the same cache and query coalescing (the C++ actor
/// referenced by `ActorId`).
///
/// Must be used inside a tokio runtime: the first query for a host spawns the
/// resolver chain onto it.
#[derive(Clone)]
pub struct GetHostByName {
    inner: Arc<Inner>,
}

impl GetHostByName {
    /// The C++ constructor's `CHECK(!options_.resolver_types.empty())`.
    pub fn new(options: Options) -> Self {
        assert!(
            !options.resolvers.is_empty(),
            "GetHostByName requires at least one resolver"
        );
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    cache: [HashMap::new(), HashMap::new()],
                    active: [HashMap::new(), HashMap::new()],
                }),
                options,
            }),
        }
    }

    /// `GetHostByNameActor::run(host, port, prefer_ipv6, promise)` as an
    /// async call: a fresh cache entry returns immediately; an expired or
    /// missing one joins the in-flight query for that (host, family) or
    /// starts it.
    pub async fn run(
        &self,
        host: &str,
        port: i32,
        prefer_ipv6: bool,
    ) -> Result<SocketAddr, Status> {
        let port = u16::try_from(port)
            .map_err(|_| Status::error_message(&format!("port {port} out of range")))?;
        let ascii_host = idn_to_ascii(host)?;
        if ascii_host.is_empty() {
            return Err(Status::error_message("Host is empty"));
        }

        let family = usize::from(prefer_ipv6);
        let receiver = {
            let mut state = self.inner.state.lock();
            if let Some(value) = state.cache[family].get(&ascii_host)
                && value.expires_at > time::now()
            {
                return value.get_ip_port(port);
            }

            let waiters = state.active[family].entry(ascii_host.clone()).or_default();
            if waiters.is_empty() {
                let inner = Arc::clone(&self.inner);
                let query_host = ascii_host.clone();
                tokio::spawn(async move {
                    inner.resolve_query(query_host, prefer_ipv6).await;
                });
            }
            let (sender, receiver) = oneshot::channel();
            waiters.push(sender);
            receiver
        };

        let ip = receiver
            .await
            .map_err(|_| Status::error_message("DNS query was dropped"))??;
        Ok(SocketAddr::new(ip, port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C++ `best_info` walk, on a family-mixed address list.
    #[test]
    fn pick_addr_prefers_requested_family() {
        let v6a: IpAddr = "2001:db8::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8::2".parse().unwrap();
        let v4a: IpAddr = "192.0.2.1".parse().unwrap();
        let v4b: IpAddr = "192.0.2.2".parse().unwrap();

        // IPv4 preferred: first IPv4 wins even with IPv6 first.
        assert_eq!(pick_addr([v6a, v4a, v4b].into_iter(), false), Some(v4a));
        // IPv6 preferred: first IPv6 wins even with IPv4 first.
        assert_eq!(pick_addr([v4a, v6a, v6b].into_iter(), true), Some(v6a));
        // No address of the preferred family: first of the other one.
        assert_eq!(pick_addr([v6a, v6b].into_iter(), false), Some(v6a));
        assert_eq!(pick_addr([v4a, v4b].into_iter(), true), Some(v4a));
        // Nothing to pick from.
        assert_eq!(pick_addr(std::iter::empty(), false), None);
    }
}
