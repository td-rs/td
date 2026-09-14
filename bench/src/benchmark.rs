//! The port of `td/utils/benchmark.h`, mirroring the C++ driver exactly:
//!
//! - `bench_n(b, n)` times `start_up_n` + `run(n)` + `tear_down`, returning
//!   the run time and the total time in seconds;
//! - `bench(b, max_time)` doubles `n` from 1 until a pass runs longer than
//!   `max_time` (or the total exceeds `3 * max_time`), then reports
//!   ops/sec: the average of the calibration pass and the measured passes,
//!   with min/max and the standard deviation, on one stderr line.

use std::time::Instant;

/// The port of C++ `td::Benchmark`. One bench object is reused across the
/// calibration and the measured passes, exactly like the C++ driver.
pub trait Benchmark {
    fn get_description(&self) -> String;

    fn start_up(&mut self) {}
    fn start_up_n(&mut self, _n: usize) {
        self.start_up();
    }

    fn tear_down(&mut self) {}

    fn run(&mut self, n: usize);
}

/// C++ `bench_n(b, n)`: `(run_time, total_time)` in seconds.
pub fn bench_n(b: &mut dyn Benchmark, n: usize) -> (f64, f64) {
    let total_start = Instant::now();
    b.start_up_n(n);
    let t0 = Instant::now();
    b.run(n);
    let t = t0.elapsed().as_secs_f64();
    b.tear_down();
    let total = total_start.elapsed().as_secs_f64();
    (t, total)
}

/// C++ `bench(b)` — the default `max_time = 1.0` overload.
pub fn bench(b: &mut dyn Benchmark) {
    bench_max(b, 1.0);
}

/// C++ `bench(b, max_time)`: the calibration loop and the report line.
pub fn bench_max(b: &mut dyn Benchmark, max_time: f64) {
    let mut n: usize = 1;
    let mut pass_time = 0.0;
    let mut total_pass_time = 0.0;
    while pass_time < max_time && total_pass_time < max_time * 3.0 && n < (1 << 30) {
        n *= 2;
        let (p, tp) = bench_n(b, n);
        pass_time = p;
        total_pass_time = tp;
    }

    let mut pass_rate = n as f64 / pass_time;
    let pass_cnt = 2;
    let mut sum = pass_rate;
    let mut square_sum = pass_rate * pass_rate;
    let mut min_rate = pass_rate;
    let mut max_rate = pass_rate;
    for _ in 1..pass_cnt {
        let (p, _) = bench_n(b, n);
        pass_rate = n as f64 / p;
        sum += pass_rate;
        square_sum += pass_rate * pass_rate;
        if pass_rate < min_rate {
            min_rate = pass_rate;
        }
        if pass_rate > max_rate {
            max_rate = pass_rate;
        }
    }
    let average = sum / pass_cnt as f64;
    let d = (square_sum / pass_cnt as f64 - average * average).sqrt();

    let description = b.get_description();
    let pad = " ".repeat(40usize.saturating_sub(description.len()));
    eprintln!(
        "Bench [{pad}{description}]: {average:.3}[{min_rate:.3}-{max_rate:.3}] ops/sec,\t{} [d = {d:.6}]",
        as_time(1.0 / average)
    );
}

/// C++ `format::as_time(seconds)`: a compact human-readable duration. The
/// unit selection is approximate (`ms` up to 999 ms, then `us`, then `ns`);
/// the ops/sec numbers are the comparison metric, this is for readability.
fn as_time(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.4} s")
    } else if seconds >= 1e-3 {
        format!("{:.4} ms", seconds * 1e3)
    } else if seconds >= 1e-6 {
        format!("{:.4} us", seconds * 1e6)
    } else {
        format!("{:.4} ns", seconds * 1e9)
    }
}
