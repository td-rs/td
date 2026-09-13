//! Port of `td/utils/benchmark.h`: the micro-benchmark harness behind the
//! C++ `benchmark/` targets (the first user is S37's `bench_actor`).
//!
//! Measurement is verbatim: the iteration count doubles until a pass exceeds
//! `max_time` (and the total exceeds `3 * max_time`), then `pass_cnt` passes
//! at that count are averaged with min/max/σ. The report line matches the
//! C++ format byte for byte apart from the log prefix, so the gate in
//! `bench/actor-gate.sh` can pair cases between the two binaries by
//! description.

use std::time::Instant;

/// The port of C++ `class Benchmark`.
pub trait Benchmark {
    /// C++ `get_description()`: the report-line key.
    fn get_description(&self) -> String;

    /// C++ `start_up()`.
    fn start_up(&mut self) {}

    /// C++ `start_up_n(n)`: per-iteration-count setup; the default defers to
    /// [`start_up`](Benchmark::start_up) exactly like C++.
    fn start_up_n(&mut self, _n: i32) {
        self.start_up();
    }
    fn tear_down(&mut self) {}

    /// C++ `run(n)`: the workload, `n` iterations.
    fn run(&mut self, n: i32);
}

/// C++ `bench_n`: one pass of `n` iterations. Returns `(run_time, total_time)`
/// in seconds.
pub fn bench_n(bench: &mut dyn Benchmark, n: i32) -> (f64, f64) {
    let total = Instant::now();
    bench.start_up_n(n);
    let run = Instant::now();
    bench.run(n);
    let run_time = run.elapsed().as_secs_f64();
    bench.tear_down();
    let total_time = total.elapsed().as_secs_f64();
    (run_time, total_time)
}
pub fn as_time(seconds: f64) -> String {
    let (value, unit) = if seconds < 1e-3 {
        (seconds * 1e6, "us")
    } else if seconds < 1.0 {
        (seconds * 1e3, "ms")
    } else {
        (seconds, "s")
    };
    format!("{value:.3}{unit}")
}

/// C++ `bench(b, max_time)`: ramp to a stable iteration count, then report
/// ops/sec (average, min, max) and per-op time over two passes.
pub fn bench(bench: &mut dyn Benchmark, max_time: f64) {
    let mut n = 1;
    let (mut pass_time, mut total_pass_time) = (0.0, 0.0);
    while pass_time < max_time && total_pass_time < max_time * 3.0 && n < (1 << 30) {
        n *= 2;
        (pass_time, total_pass_time) = bench_n(bench, n);
    }
    pass_time = n as f64 / pass_time;

    let pass_cnt = 2;
    let mut sum = pass_time;
    let mut square_sum = pass_time * pass_time;
    let mut min_pass_time = pass_time;
    let mut max_pass_time = pass_time;

    for _ in 1..pass_cnt {
        pass_time = n as f64 / bench_n(bench, n).0;
        sum += pass_time;
        square_sum += pass_time * pass_time;
        min_pass_time = min_pass_time.min(pass_time);
        max_pass_time = max_pass_time.max(pass_time);
    }
    let average = sum / pass_cnt as f64;
    let d = (square_sum / pass_cnt as f64 - average * average).sqrt();

    let description = bench.get_description();
    let pad = " ".repeat(40usize.saturating_sub(description.len()));
    eprintln!(
        "Bench [{pad}{description}]: {average:.3}[{min_pass_time:.3}-{max_pass_time:.3}] ops/sec,\t{} [d = {d:.6}]",
        as_time(1.0 / average)
    );
}
