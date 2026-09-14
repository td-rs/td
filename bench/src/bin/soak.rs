//! S80 soak: an allocation-sensitive mixed workload — actor
//! create/destroy + ring messaging, binlog-KV and in-memory KV churn, and
//! buffer-builder churn — with periodic RSS sampling. The S80 gate is the
//! absence of pathological growth: RSS over the last quarter of the run
//! must stay within a bounded margin of the first quarter (after warmup),
//! i.e. the port reaches a steady-state heap instead of leaking.
//!
//! Each tick runs one full ConcurrentScheduler lifecycle, exactly the
//! `bench_actor` CreateActorBench shape: register pre-start, wire+sends
//! under `get_main_guard`, and the last actor's `tear_down` finishes the
//! scheduler so `run_main` returns.
//!
//! Usage: `soak [seconds]` (default 60). Exits non-zero on gate failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use tdactor::{Actor, ActorId, ConcurrentScheduler, Scheduler, actor_messages, send_closure};
use tddb::binlog_key_value::BinlogKeyValue;
use tddb::seq_key_value::SeqKeyValue;
use tddb::{DbKey, sqlite_db};
use tdutils::buffer::{BufferBuilder, BufferWriter, ChainBufferWriter};

fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| {
            statm
                .split_whitespace()
                .next()
                .and_then(|resident_pages| resident_pages.parse::<u64>().ok())
        })
        .map(|pages| pages * 4096)
        .expect("statm resident size")
}

/// One ring link: forwards a message to the next actor; `tear_down`
/// (triggered by the Stop message) decrements the tick's live count, and
/// the last one finishes the scheduler so the pump loop exits — the
/// `bench_actor` CreateActorBench lifecycle.
struct PassActor {
    next: Option<ActorId<PassActor>>,
    live: Arc<AtomicI32>,
}

impl Actor for PassActor {
    fn tear_down(&mut self) {
        if self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            Scheduler::instance().finish();
        }
    }
}

actor_messages! {
    enum PassMessage for PassActor {
        Pass { seq: usize, } => pass(seq),
        SetNext { next: ActorId<PassActor>, } => set_next(next),
        Stop { seq: usize, } => stop_self(seq),
    }
}

impl PassActor {
    fn pass(&mut self, _seq: usize) {
        if let Some(next) = &self.next {
            send_closure(next, PassMessage::Pass { seq: 0 });
        }
    }

    fn set_next(&mut self, next: ActorId<PassActor>) {
        self.next = Some(next);
    }

    fn stop_self(&mut self, _seq: usize) {
        self.stop();
    }
}

/// One actor-churn tick: a fresh scheduler, a fresh ring of `ring_n`
/// actors wired into a loop, `ticks` messages pushed around it, then every
/// link stopped — the register/destroy path is the allocation-sensitive
/// one (the S37 CreateActor gate).
fn actor_tick(ring_n: usize, ticks: usize) {
    let live = Arc::new(AtomicI32::new(ring_n as i32));
    let mut sched = ConcurrentScheduler::new(0);

    let mut actors = Vec::with_capacity(ring_n);
    for _ in 0..ring_n {
        let own = sched.create_actor_unsafe(
            0,
            "SoakPass",
            PassActor {
                next: None,
                live: Arc::clone(&live),
            },
        );
        actors.push(own);
    }
    {
        let _guard = sched.get_main_guard();
        for i in 0..ring_n {
            let next = actors[(i + 1) % ring_n].get();
            send_closure(&actors[i].get(), PassMessage::SetNext { next });
        }
    }
    sched.start();
    {
        let _guard = sched.get_main_guard();
        for _ in 0..ticks {
            send_closure(&actors[0].get(), PassMessage::Pass { seq: 0 });
        }
        for actor in &actors {
            send_closure(&actor.get(), PassMessage::Stop { seq: 0 });
        }
    }
    while sched.run_main(10.0) {}
    sched.finish();
}

/// KV + buffer churn state.
struct KvChurn {
    seq: SeqKeyValue,
    binlog: BinlogKeyValue,
    i: usize,
}

impl KvChurn {
    fn new(path: &str) -> Self {
        let _ = sqlite_db::destroy(path);
        let mut binlog = BinlogKeyValue::default();
        binlog.init(path, DbKey::empty()).ensure();
        KvChurn {
            seq: SeqKeyValue::default(),
            binlog,
            i: 0,
        }
    }

    fn tick(&mut self) {
        for j in 0..128 {
            let key = format!("k{}", j % 16);
            let value = format!("{}", self.i * 128 + j);
            self.seq.set(&key, &value);
            self.binlog.set(&key, &value);
        }
        // Buffer churn: build, frame, extract — the transport-adjacent path.
        let mut builder = BufferBuilder::from_slice_with_reserve(b"seed", 8, 64);
        for j in 0..8 {
            builder.append(format!("payload-{}-{}", self.i, j).as_bytes());
        }
        let mut writer = ChainBufferWriter::new();
        writer.append(b"0123456789abcdef0123456789abcdef", 0);
        drop(writer.extract_reader());

        self.i += 1;
        if self.i % 256 == 0 {
            // Reopen cycle: fd + binlog replay churn.
            self.binlog.close();
            self.binlog
                .init("test_soak_binlog", DbKey::empty())
                .ensure();
        }
    }
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let _ = sqlite_db::destroy("test_soak_binlog");
    let mut churn = KvChurn::new("test_soak_binlog");

    let start = Instant::now();
    let mut samples: Vec<u64> = Vec::new();
    let mut ticks: usize = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        actor_tick(8, 32);
        churn.tick();
        ticks += 1;
        samples.push(rss_bytes());
        std::thread::sleep(Duration::from_millis(50));
    }

    let quarter = samples.len() / 4;
    let avg = |range: std::ops::Range<usize>| -> f64 {
        let slice = &samples[range];
        slice.iter().sum::<u64>() as f64 / slice.len().max(1) as f64
    };
    let warmup_q = avg(0..quarter.max(1));
    let final_q = avg(samples.len().saturating_sub(quarter)..samples.len());
    let min = samples.iter().min().copied().unwrap_or(0);
    let max = samples.iter().max().copied().unwrap_or(0);
    let growth = final_q - warmup_q;
    let growth_pct = if warmup_q > 0.0 {
        growth / warmup_q * 100.0
    } else {
        0.0
    };
    let growth_mib = growth / 1048576.0;

    eprintln!(
        "soak: {seconds}s, {ticks} ticks, RSS min {:.1} MiB / max {:.1} MiB, warmup quarter {:.1} MiB -> final quarter {:.1} MiB (growth {growth_mib:+.1} MiB, {growth_pct:+.1}%)",
        min as f64 / 1048576.0,
        max as f64 / 1048576.0,
        warmup_q / 1048576.0,
        final_q / 1048576.0,
    );

    // Gate: steady state — final-quarter RSS within 10% (+4 MiB slack) of
    // the warmup quarter. Growth beyond that flags a leak.
    if growth > warmup_q * 0.10 + 4.0 * 1048576.0 {
        eprintln!("soak: FAIL — pathological RSS growth");
        std::process::exit(1);
    }
    eprintln!("soak: PASS — bounded heap");
}
