//! S47 verification — the C++ `idn_to_ascii` vectors from
//! `tdutils/test/misc.cpp` and behavior tests for the ported
//! `GetHostByNameActor`: result/error caching, in-flight query coalescing,
//! resolver-chain fallback, per-family caches, and port stamping.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tdnet::dns::{GetHostByName, NativeResolver, Options, Resolver};
use tdnet::idn_to_ascii;
use tdutils::status::Status;

const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

fn error(message: &str) -> Status {
    Status::error_message(message)
}

/// A resolver a test scripts: pops one queued outcome per call (reusing the
/// last when exhausted), records every queried host, and delays before
/// answering so coalescing has a window.
struct ScriptedResolver {
    delay: Duration,
    outcomes: Mutex<VecDeque<Result<IpAddr, Status>>>,
    hosts: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl ScriptedResolver {
    fn new(outcomes: Vec<Result<IpAddr, Status>>) -> Self {
        Self {
            delay: Duration::ZERO,
            outcomes: Mutex::new(outcomes.into()),
            hosts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn with_delay(delay: Duration, outcomes: Vec<Result<IpAddr, Status>>) -> Self {
        Self {
            delay,
            ..Self::new(outcomes)
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Resolver for ScriptedResolver {
    fn resolve(&self, host: &str, _prefer_ipv6: bool) -> tdnet::dns::ResolveFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.hosts.lock().push(host.to_owned());
        let outcome = {
            let mut outcomes = self.outcomes.lock();
            if outcomes.len() > 1 {
                outcomes.pop_front().expect("non-empty")
            } else {
                outcomes.front().expect("non-empty").clone()
            }
        };
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            outcome
        })
    }
}

/// The C++ resolver that answers per family — for the prefer_ipv6 cache-split
/// test.
struct FamilyResolver {
    calls: AtomicUsize,
}

impl Resolver for FamilyResolver {
    fn resolve(&self, _host: &str, prefer_ipv6: bool) -> tdnet::dns::ResolveFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let ip = if prefer_ipv6 { V6 } else { V4 };
        Box::pin(async move { Ok(ip) })
    }
}

impl FamilyResolver {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn service(resolvers: Vec<Arc<dyn Resolver>>) -> GetHostByName {
    GetHostByName::new(Options {
        resolvers,
        ..Options::default()
    })
}

#[test]
fn idn_to_ascii_cxx_vectors() {
    // tdutils/test/misc.cpp TEST(Misc, idn_to_ascii). The C++ case
    // `idn_to_ascii("\xc0").is_error()` needs an invalid-UTF-8 host, which a
    // Rust `&str` cannot hold; that path is unreachable by construction.
    const VECTORS: &[(&str, &str)] = &[
        (
            "::::::::::::::::::::::::::::::::::::::@/",
            "::::::::::::::::::::::::::::::::::::::@/",
        ),
        ("", ""),
        ("%30", "%30"),
        ("127.0.0.1", "127.0.0.1"),
        ("fe80::", "fe80::"),
        (
            "fe80:0:0:0:200:f8ff:fe21:67cf",
            "fe80:0:0:0:200:f8ff:fe21:67cf",
        ),
        (
            "2001:0db8:11a3:09d7:1f34:8a2e:07a0:765d",
            "2001:0db8:11a3:09d7:1f34:8a2e:07a0:765d",
        ),
        ("::ffff:192.0.2.1", "::ffff:192.0.2.1"),
        ("ABCDEF", "abcdef"),
        ("abcdef", "abcdef"),
        ("abæcdöef", "xn--abcdef-qua4k"),
        ("schön", "xn--schn-7qa"),
        ("ยจฆฟคฏข", "xn--22cdfh1b8fsa"),
        ("☺", "xn--74h"),
        ("правда", "xn--80aafi6cg"),
        ("büücher", "xn--bcher-kvaa"),
        ("BüüCHER", "xn--bcher-kvaa"),
        ("bücüher", "xn--bcher-kvab"),
        ("bücherü", "xn--bcher-kvae"),
        ("ýbücher", "xn--bcher-kvaf"),
        ("übücher", "xn--bcher-jvab"),
        ("bücher.tld", "xn--bcher-kva.tld"),
        ("кто.рф", "xn--j1ail.xn--p1ai"),
        ("wіkіреdіа.org", "xn--wkd-8cdx9d7hbd.org"),
        ("cnwin2k8中国.avol.com", "xn--cnwin2k8-sd0mx14e.avol.com"),
        (
            "win-2k12r2-addc.阿伯测阿伯测ad.hai.com",
            "win-2k12r2-addc.xn--ad-tl3ca3569aba8944eca.hai.com",
        ),
        ("✌.ws", "xn--7bi.ws"),
        ("⛧", "xn--59h"),
        ("--рф.рф", "xn-----mmcq.xn--p1ai"),
    ];
    for (host, expected) in VECTORS {
        let got = idn_to_ascii(host).expect("vector converts");
        assert_eq!(&got, expected, "idn_to_ascii({host:?})");
    }
}

#[test]
fn idn_to_ascii_rejects_overlong_host() {
    // > 255*4 bytes of non-ASCII input hits the C++ length bound.
    let host = "ä".repeat(255 * 4);
    assert!(idn_to_ascii(&host).is_err());
}

#[tokio::test]
async fn resolves_localhost_through_native_resolver() {
    let address = GetHostByName::new(Options::default())
        .run("localhost", 80, false)
        .await
        .expect("localhost resolves");
    assert_eq!(address.port(), 80);
    assert!(address.ip().is_loopback());
}

#[tokio::test]
async fn native_resolver_handles_literals_without_dns() {
    let resolver = NativeResolver;
    let bracketed = resolver.resolve("[::1]", false).await.expect("[::1]");
    assert_eq!(bracketed, V6);
    let v4 = resolver
        .resolve("127.0.0.1", false)
        .await
        .expect("127.0.0.1");
    assert_eq!(v4, V4);
    assert_eq!(
        resolver
            .resolve("[not-an-ip]", false)
            .await
            .unwrap_err()
            .message(),
        "\"[not-an-ip]\" is not a valid IPv6 address"
    );
    assert_eq!(
        resolver.resolve("", true).await.unwrap_err().message(),
        "Host is empty"
    );
}

#[tokio::test]
async fn caches_result_and_stamps_port_at_delivery() {
    let resolver = Arc::new(ScriptedResolver::new(vec![Ok(V4)]));
    let service = service(vec![resolver.clone()]);

    let first = service.run("telegram.org", 80, false).await.expect("ok");
    let second = service.run("telegram.org", 443, false).await.expect("ok");

    assert_eq!(first.ip(), V4);
    assert_eq!(first.port(), 80);
    assert_eq!(second.ip(), V4);
    assert_eq!(second.port(), 443);
    assert_eq!(resolver.calls(), 1, "second lookup must hit the cache");
}

#[tokio::test]
async fn caches_errors_negatively() {
    let resolver = Arc::new(ScriptedResolver::new(vec![Err(error("no such host"))]));
    let service = service(vec![resolver.clone()]);

    let first = service.run("gone.invalid", 80, false).await;
    let second = service.run("gone.invalid", 80, false).await;

    assert_eq!(first.unwrap_err().message(), "no such host");
    assert_eq!(second.unwrap_err().message(), "no such host");
    assert_eq!(resolver.calls(), 1, "cached errors must not re-query");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coalesces_concurrent_queries_for_same_host() {
    let resolver = Arc::new(ScriptedResolver::with_delay(
        Duration::from_millis(200),
        vec![Ok(V4)],
    ));
    let service = service(vec![resolver.clone()]);

    let clients: Vec<_> = (0..8)
        .map(|i| {
            let service = service.clone();
            tokio::spawn(async move {
                let address = service
                    .run("coalesce.test", 1000 + i, false)
                    .await
                    .expect("ok");
                (i, address)
            })
        })
        .collect();
    for client in clients {
        let (i, address) = client.await.expect("client task");
        assert_eq!(address.ip(), V4, "client {i}");
        assert_eq!(
            address.port(),
            1000 + i as u16,
            "each caller keeps its own port"
        );
    }

    assert_eq!(
        resolver.calls(),
        1,
        "concurrent lookups must share one query"
    );
}

#[tokio::test]
async fn falls_back_to_next_resolver_on_error() {
    let failing = Arc::new(ScriptedResolver::new(vec![Err(error("resolver down"))]));
    let working = Arc::new(ScriptedResolver::new(vec![Ok(V6)]));
    let service = service(vec![failing.clone(), working.clone()]);

    let address = service.run("telegram.org", 443, false).await.expect("ok");

    assert_eq!(address.ip(), V6);
    assert_eq!(failing.calls(), 1);
    assert_eq!(
        working.calls(),
        1,
        "the failed resolver must not be retried"
    );
    // The fallback's success is what gets cached.
    let cached = service.run("telegram.org", 443, false).await.expect("ok");
    assert_eq!(cached.ip(), V6);
    assert_eq!(failing.calls(), 1, "cache hit must not re-run the chain");
}

#[tokio::test]
async fn exhausted_chain_reports_last_error() {
    let first = Arc::new(ScriptedResolver::new(vec![Err(error("first"))]));
    let second = Arc::new(ScriptedResolver::new(vec![Err(error("second"))]));
    let service = service(vec![first.clone(), second.clone()]);

    let result = service.run("telegram.org", 443, false).await;

    assert_eq!(result.unwrap_err().message(), "second");
    assert_eq!(first.calls(), 1);
    assert_eq!(second.calls(), 1);
}

#[tokio::test]
async fn separates_caches_per_prefer_ipv6() {
    let resolver = Arc::new(FamilyResolver {
        calls: AtomicUsize::new(0),
    });
    let service = service(vec![resolver.clone()]);

    let v4 = service.run("dual.test", 1, false).await.expect("v4");
    let v6 = service.run("dual.test", 1, true).await.expect("v6");

    assert_eq!(v4.ip(), V4);
    assert_eq!(v6.ip(), V6);
    assert_eq!(resolver.calls(), 2, "each family resolves once");

    service.run("dual.test", 1, false).await.expect("cached v4");
    service.run("dual.test", 1, true).await.expect("cached v6");
    assert_eq!(resolver.calls(), 2, "both families served from cache");
}

#[tokio::test]
async fn ok_cache_expires_after_ok_timeout() {
    let resolver = Arc::new(ScriptedResolver::new(vec![Ok(V4)]));
    let service = GetHostByName::new(Options {
        resolvers: vec![resolver.clone()],
        ok_timeout: Duration::from_millis(100),
        error_timeout: Duration::from_secs(5),
    });

    service.run("telegram.org", 80, false).await.expect("ok");
    assert_eq!(resolver.calls(), 1);
    service
        .run("telegram.org", 80, false)
        .await
        .expect("cached");
    assert_eq!(resolver.calls(), 1);

    tokio::time::sleep(Duration::from_millis(150)).await;
    service
        .run("telegram.org", 80, false)
        .await
        .expect("re-resolved");
    assert_eq!(resolver.calls(), 2);
}

#[tokio::test]
async fn error_cache_expires_after_error_timeout() {
    let resolver = Arc::new(ScriptedResolver::new(vec![Err(error("no such host"))]));
    let service = GetHostByName::new(Options {
        resolvers: vec![resolver.clone()],
        ok_timeout: Duration::from_secs(5),
        error_timeout: Duration::from_millis(100),
    });

    service
        .run("gone.invalid", 80, false)
        .await
        .expect_err("fails");
    assert_eq!(resolver.calls(), 1);
    service
        .run("gone.invalid", 80, false)
        .await
        .expect_err("cached error");
    assert_eq!(resolver.calls(), 1);

    tokio::time::sleep(Duration::from_millis(150)).await;
    service
        .run("gone.invalid", 80, false)
        .await
        .expect_err("re-queried");
    assert_eq!(resolver.calls(), 2);
}

#[tokio::test]
async fn run_validates_port_and_host() {
    let service = service(vec![Arc::new(ScriptedResolver::new(vec![Ok(V4)]))]);

    assert_eq!(
        service
            .run("telegram.org", 65536, false)
            .await
            .unwrap_err()
            .message(),
        "port 65536 out of range"
    );
    assert_eq!(
        service
            .run("telegram.org", -1, false)
            .await
            .unwrap_err()
            .message(),
        "port -1 out of range"
    );
    assert_eq!(
        service.run("", 80, false).await.unwrap_err().message(),
        "Host is empty"
    );
    assert_eq!(
        service
            .run("bücher", 80, false)
            .await
            .expect("idn host")
            .port(),
        80
    );
}

#[tokio::test]
async fn run_normalizes_idn_host_before_cache_and_resolvers() {
    let resolver = Arc::new(ScriptedResolver::new(vec![Ok(V4)]));
    let service = service(vec![resolver.clone()]);

    service.run("кто.рф", 80, false).await.expect("ok");
    // The resolver must have seen the punycode form…
    assert_eq!(resolver.hosts.lock().as_slice(), ["xn--j1ail.xn--p1ai"]);
    // …and a repeated lookup hits the same cache entry. (Uppercase
    // "КТО.РФ" is a *different* punycode label: C++ applies no nameprep or
    // Unicode case-folding.)
    service.run("кто.рф", 443, false).await.expect("cache hit");
    assert_eq!(resolver.calls(), 1, "identical hosts share one cache entry");
}

#[tokio::test]
async fn panicking_resolver_flushes_waiters_with_error() {
    struct PanickingResolver;
    impl Resolver for PanickingResolver {
        fn resolve(&self, _host: &str, _prefer_ipv6: bool) -> tdnet::dns::ResolveFuture {
            Box::pin(async { panic!("resolver exploded") })
        }
    }

    let service = service(vec![Arc::new(PanickingResolver)]);

    let result = service.run("telegram.org", 80, false).await;

    assert_eq!(
        result.unwrap_err().message(),
        "DNS resolver panicked: resolver exploded"
    );
}
