//! S45 verification — echo tests over the ported TCP client/server
//! (`TcpListener` + `connect`), mirroring the C++ tests that drive
//! `td::TcpListener` on `ConcurrentScheduler` with N concurrent clients.

use std::time::{Duration, Instant};

use tdnet::tcp::{AcceptCallback, TcpListener, TcpStream, connect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The C++ tests hammer the listener with 8 concurrent clients.
const N: usize = 8;

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// The C++ `Server : TcpListener::Callback` echo test — each accepted
/// connection gets its own handler (an actor in C++, a spawned task here).
struct Echo;

impl AcceptCallback for Echo {
    async fn accept(&mut self, stream: TcpStream) {
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(stream);
            let _ = tokio::io::copy(&mut read, &mut write).await;
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_eight_clients() {
    let listener = TcpListener::open("127.0.0.1", 0).expect("bind");
    let address = listener.local_addr().expect("local_addr");
    assert_ne!(address.port(), 0, "port 0 must bind an ephemeral port");
    let server = tokio::spawn(listener.serve(Echo));

    let sizes = [0usize, 1, 1024, 65536, 262144];
    let clients = (0..N).map(|i| {
        let size = sizes[i % sizes.len()];
        tokio::spawn(async move {
            let data = payload(size);
            let stream = connect("127.0.0.1", address.port() as i32)
                .await
                .expect("connect");
            // Write and echo-read concurrently: a same-socket echo deadlocks
            // once both direction's buffers fill (payloads up to 256 KiB).
            let (mut read, mut write) = tokio::io::split(stream);
            let writer = tokio::spawn(async move {
                write.write_all(&payload(size)).await.expect("write_all");
            });
            let mut echoed = vec![0u8; size];
            read.read_exact(&mut echoed).await.expect("read_exact");
            writer.await.expect("writer task");
            assert_eq!(echoed, data, "echo must be byte-identical (size {size})");
        })
    });
    for client in clients {
        client.await.expect("client task");
    }

    server.abort();
}

#[tokio::test]
async fn open_rejects_occupied_port() {
    let held = TcpListener::open("127.0.0.1", 0).expect("bind");
    let port = held.local_addr().expect("local_addr").port();
    assert!(TcpListener::open("127.0.0.1", port as i32).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_retries_bind_after_backoff() {
    // Hold the port: the serve loop must hit the C++ `set_timeout_in(5)`
    // retry path (compressed to 50 ms here) instead of failing outright.
    let held = TcpListener::open("127.0.0.1", 0).expect("bind");
    let port = held.local_addr().expect("local_addr").port();
    let server = tokio::spawn(TcpListener::serve_with_backoff(
        "127.0.0.1",
        port as i32,
        Duration::from_millis(50),
        Echo,
    ));
    tokio::time::sleep(Duration::from_millis(150)).await; // at least one failed bind + retry
    drop(held);

    // The next retry binds; poll until the echo server answers.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match connect("127.0.0.1", port as i32).await {
            Ok(mut stream) => {
                stream.write_all(b"ping").await.expect("write");
                let mut echoed = [0u8; 4];
                stream.read_exact(&mut echoed).await.expect("read");
                assert_eq!(&echoed, b"ping");
                break;
            }
            Err(_) => {
                assert!(Instant::now() < deadline, "server never came up");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    server.abort();
}
