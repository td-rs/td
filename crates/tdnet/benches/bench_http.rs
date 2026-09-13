//! Port of `benchmark/bench_http.cpp` — two clients hammering a loopback
//! server over persistent connections, plus the ported `wget` entry point.
//! Run with `cargo bench -p tdnet --bench bench_http`.

use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One shot per connection: a fixed 128-byte 200 response, keep-alive.
static RESPONSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Length: 128\r\nConnection: keep-alive\r\n\r\n";

async fn spawn_bench_server(listener: TcpListener) {
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = serve_loopback(socket).await;
            });
        }
    });
}

async fn serve_loopback(mut socket: TcpStream) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        // read one request head
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf.clear();
        let mut response = Vec::with_capacity(RESPONSE_HEAD.len() + 128);
        response.extend_from_slice(RESPONSE_HEAD.as_bytes());
        response.extend_from_slice(&[b'x'; 128]);
        socket.write_all(&response).await?;
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    spawn_bench_server(listener).await;

    let url = format!("http://{addr}/");
    const CLIENTS: usize = 2;
    const QUERIES_PER_CLIENT: usize = 50_000;

    // The persistent-connection stack under Wget (what C++ HttpOutboundConnection measures).
    let client = Arc::new(reqwest::Client::new());
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..CLIENTS {
        let client = Arc::clone(&client);
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..QUERIES_PER_CLIENT {
                client
                    .get(&url)
                    .send()
                    .await
                    .expect("query")
                    .bytes()
                    .await
                    .expect("body");
            }
        }));
    }
    for task in tasks {
        task.await.expect("client task");
    }
    let elapsed = start.elapsed();
    let total = (CLIENTS * QUERIES_PER_CLIENT) as f64;
    println!(
        "bench_http: {total} queries over {} pooled connections in {:.3}s ({:.0} req/s)",
        CLIENTS,
        elapsed.as_secs_f64(),
        total / elapsed.as_secs_f64()
    );

    // The ported entry point: a fresh client and connection per call,
    // mirroring one C++ Wget actor per request.
    const WGET_QUERIES: usize = 1_000;
    let start = Instant::now();
    for _ in 0..WGET_QUERIES {
        tdnet::wget::wget(
            &url,
            tdnet::wget::WgetParams {
                timeout_in: std::time::Duration::from_secs(10),
                ..Default::default()
            },
        )
        .await
        .move_as_ok();
    }
    let elapsed = start.elapsed();
    println!(
        "wget: {WGET_QUERIES} queries (fresh connection each) in {:.3}s ({:.0} req/s)",
        elapsed.as_secs_f64(),
        WGET_QUERIES as f64 / elapsed.as_secs_f64()
    );
}
