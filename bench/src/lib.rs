//! Port of `td/utils/benchmark.h`: the `Benchmark` base class and the
//! `bench`/`bench_n` drivers, with the exact C++ measurement methodology —
//! double `n` until a pass exceeds `max_time` seconds, then report the
//! average of the calibration pass plus the measured passes.
//!
//! The C++ harness lives in `tdutils`; the Rust port lives here for now
//! because the first consumer is the S37 `bench_actor` gate. Hoist it into
//! `tdutils` if a library crate ever needs it (S79 ports the remaining
//! `benchmark/` targets).

pub mod benchmark;
