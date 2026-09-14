//! Port of `benchmark/bench_http.cpp` (S79): two clients issue 100 000
//! sequential `GET /` requests each against a local HTTP server (the C++
//! `bench_http_server_fast` on 127.0.0.1:8082, used for both sides of the
//! pairing), one request in flight per connection — like the C++
//! `HttpClient` actor, which re-issues only after each response.
//!
//! Divergences from the C++ benchmark, documented in `bench/README.md`:
//!
//! - The C++ bench drives `td::HttpOutboundConnection` actors (persistent
//!   raw-socket HTTP); the Rust port drives a pooled `reqwest`/hyper client
//!   — the same transport layer the S48 `Wget` port uses (the persistent
//!   -connection actor is not ported yet). Keep-alive pooling preserves the
//!   measured shape: one established connection per client, request ->
//!   response -> request.
//! - Like the C++ bench (no `td::bench` driver), the metric is the binary's
//!   wall-clock runtime, compared side by side.

use std::time::Instant;

const CLIENTS: usize = 2;
const QUERIES: usize = 100_000;

fn main() {
    // The C++ bench hardcodes 8082; an optional argv[1] overrides the port
    // here (8082 is taken by an unrelated local service on this machine).
    let port: u16 = std::env::args()
        .nth(1)
        .map(|p| p.parse().expect("port"))
        .unwrap_or(8082);
    let url = format!("http://127.0.0.1:{port}/");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(CLIENTS)
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async move {
        let mut tasks = Vec::new();
        let start = Instant::now();
        for _ in 0..CLIENTS {
            let url = url.clone();
            tasks.push(tokio::spawn(async move {
                let client = reqwest::Client::builder()
                    .pool_max_idle_per_host(1)
                    .build()
                    .expect("client");
                for _ in 0..QUERIES {
                    let response = client.get(&url).send().await.expect("request");
                    let server_header = response
                        .headers()
                        .get("Server")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let body = response.bytes().await.expect("body");
                    if &body[..] != b"hello world" {
                        panic!(
                            "unexpected response from Server header {server_header:?}: {} bytes",
                            body.len()
                        );
                    }
                }
            }));
        }
        for task in tasks {
            task.await.expect("client task");
        }
        let elapsed = start.elapsed();
        eprintln!(
            "bench_http: {CLIENTS} clients x {QUERIES} sequential gets in {:.3}s ({:.0} ops/sec)",
            elapsed.as_secs_f64(),
            (CLIENTS * QUERIES) as f64 / elapsed.as_secs_f64(),
        );
    });
}
