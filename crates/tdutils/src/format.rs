//! `td::format` helpers (tdutils/td/utils/format.h) plus the
//! `StringBuilder::FixedDouble` family (tdutils/td/utils/StringBuilder.h:118-128)
//! that backs td's plain-double log printing.

use std::fmt;

/// `StringBuilder::FixedDouble` (StringBuilder.h:118-126): fixed-point with an
/// explicit precision. The implicit `sb << double` conversion (StringBuilder.h
/// :128) is [`fixed_double`] — precision 6.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FixedDouble(pub f64, pub usize);

/// The `sb << double` form (StringBuilder.h:127-129): `FixedDouble(x, 6)`.
pub fn fixed_double(d: f64) -> FixedDouble {
    FixedDouble(d, 6)
}

impl fmt::Display for FixedDouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.*}", self.1, self.0)
    }
}

/// `format::Time` (format.h:160-167) — a seconds value rendered through
/// [`as_time`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Time(f64);

/// `format::as_time` (format.h:169-183): the largest of ns/us/ms/s whose
/// threshold the value exceeds, one decimal place, unit suffix — the
/// `176.4us` / `62.0s` shapes in td logs.
pub fn as_time(seconds: f64) -> Time {
    Time(seconds)
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const DURATIONS: [(&str, f64); 4] = [("ns", 1e-9), ("us", 1e-6), ("ms", 1e-3), ("s", 1.0)];
        let mut i = 0;
        while i + 1 < DURATIONS.len() && self.0 > 10.0 * DURATIONS[i + 1].1 {
            i += 1;
        }
        write!(
            f,
            "{:.1}{}",
            FixedDouble(self.0 / DURATIONS[i].1, 1),
            DURATIONS[i].0
        )
    }
}

/// `td::Timer` (tdutils/td/utils/Timer.h) — the pause/resume machinery is
/// unused by the log sites; the `<< timer` suffix renders as
/// `" in " + as_time(elapsed)` (Timer.cpp:45-47).
pub struct Timer {
    start_at: f64,
}

impl Timer {
    pub fn start() -> Self {
        Timer {
            start_at: crate::time::now_cached(),
        }
    }

    pub fn elapsed(&self) -> f64 {
        crate::time::now_cached() - self.start_at
    }
}

impl fmt::Display for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, " in {}", as_time(self.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::{FixedDouble, Timer, as_time, fixed_double};

    /// The C++ samples from the SessionConnection.cpp:1150 /
    /// Td.cpp:666-671 log lines (cmp7-cpp run, 2026-09-30).
    #[test]
    fn as_time_shapes() {
        assert_eq!(as_time(0.0001764).to_string(), "176.4us");
        assert_eq!(as_time(0.0002216).to_string(), "221.6us");
        assert_eq!(as_time(0.000138).to_string(), "138.0us");
        assert_eq!(as_time(62.0).to_string(), "62.0s");
        assert_eq!(as_time(136.962929).to_string(), "137.0s");
    }

    #[test]
    fn fixed_double_shapes() {
        // C++ `sb << 2.0` prints fixed-6 ("RTT = 2.000000").
        assert_eq!(fixed_double(2.0).to_string(), "2.000000");
        assert_eq!(fixed_double(136.962929).to_string(), "136.962929");
        assert_eq!(fixed_double(0.0).to_string(), "0.000000");
        assert_eq!(FixedDouble(1.5, 1).to_string(), "1.5");
    }

    /// The `<< timer` suffix renders as " in " + as_time(elapsed)
    /// (Timer.cpp:45-47) — the Td.cpp close-path shapes
    /// ("StateManager was cleared in 138.0us"). The exact human
    /// rendering is as_time's tested territory; here we pin the wiring.
    #[test]
    fn timer_suffix() {
        let timer = Timer {
            start_at: super::super::time::now_cached() - 0.000138,
        };
        let rendered = timer.to_string();
        assert!(rendered.starts_with(" in "));
        let body = &rendered[4..];
        assert!(
            body.ends_with("ns")
                || body.ends_with("us")
                || body.ends_with("ms")
                || body.ends_with('s'),
            "unexpected timer render: {rendered}"
        );
        assert!(timer.elapsed() >= 0.0);
    }
}
