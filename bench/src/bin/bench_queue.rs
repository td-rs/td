//! Port of `benchmark/bench_queue.cpp` (S79): the `QueueBenchmark2` shape —
//! a client thread and a server thread exchanging encoded
//! `(connection << 24 | seq_no)` values through two queues, `connections_n`
//! logical connections at a time — over the queue primitive the Rust port
//! actually uses, `crossbeam_channel`.
//!
//! Divergences from the C++ benchmark, documented in `bench/README.md`:
//!
//! - The other C++ rows (`PipeQueue`, `SemQueue`, `EventfdQueue`,
//!   `BufferQueue`, `BufferedFdQueue`, `FdQueue`, `PollQueue`, `VarQueue`,
//!   `SemCheatQueue`, `RingBenchmark`) measure OS-primitive mailbox
//!   transports (eventfd/poll, pipes, semaphores, fds) that the Rust
//!   rewrite replaced with crossbeam channels and tokio — those topologies
//!   do not exist on the Rust side, so the rows compare the two chosen
//!   transports head to head: `crossbeam_channel(N)` vs
//!   `td::InfBackoffQueue(N)` / `td::MpscPollableQueue(N)` (eventfd).

use bench::benchmark::{Benchmark, bench};
use crossbeam_channel::unbounded;

/// The C++ `QueueBenchmark2` over `crossbeam_channel` (the C++
/// `writer_put`/`reader_wait`/`reader_get_unsafe` batching collapses to one
/// `send`/`recv` per value).
struct QueueBench2 {
    connections_n: usize,
}

impl Benchmark for QueueBench2 {
    fn get_description(&self) -> String {
        format!("crossbeam_channel({})", self.connections_n)
    }

    fn run(&mut self, n: usize) {
        let connections_n = self.connections_n;
        let queries_n = n.div_ceil(connections_n);

        // client queue: server -> client; server queue: client -> server
        let (client_tx, client_rx) = unbounded::<i32>();
        let (server_tx, server_rx) = unbounded::<i32>();

        let server = std::thread::spawn(move || {
            let mut server_conn = vec![0i64; connections_n];
            let mut server_active_connections = connections_n;
            while server_active_connections > 0 {
                let value = server_rx.recv().expect("server queue closed");
                let no = (value & 0x00FF_FFFF) as i64;
                let co = ((value as u32) >> 24) as usize;
                assert!(co < connections_n);
                assert_eq!(no, server_conn[co], "out-of-order server value");
                server_conn[co] += 1;

                // The C++ writer_put + writer_flush pair.
                client_tx.send(value).expect("client queue closed");
                if no + 1 >= queries_n as i64 {
                    server_active_connections -= 1;
                }
            }
        });

        let mut client_conn = vec![0i64; connections_n];
        let mut client_active_connections = connections_n;
        assert!(queries_n < (1 << 24));
        for i in 0..connections_n {
            server_tx
                .send((i as i32) << 24)
                .expect("server queue closed");
        }

        while client_active_connections > 0 {
            let value = client_rx.recv().expect("client queue closed");
            let no = (value & 0x00FF_FFFF) as i64;
            let co = ((value as u32) >> 24) as usize;
            assert!(co < connections_n);
            assert_eq!(no, client_conn[co], "out-of-order client value");
            client_conn[co] += 1;
            if no + 1 < queries_n as i64 {
                server_tx
                    .send(value.wrapping_add(1))
                    .expect("server queue closed");
            } else {
                client_active_connections -= 1;
            }
        }

        server.join().expect("server thread panicked");
    }
}

fn main() {
    for connections_n in [1usize, 10, 100] {
        bench(&mut QueueBench2 { connections_n });
    }
}
