//! Port of `td/utils/Time.h` + `Time.cpp` (+ the `Timer` of
//! `td/utils/Timer.h`) on `std::time::Instant` and tokio for sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The clock offset added to the monotonic time (`td::time_diff`), stored as
/// `f64` bits so the C++ compare-exchange adjustment loop ports directly.
static TIME_DIFF: AtomicU64 = AtomicU64::new(0.0f64.to_bits());

fn monotonic_seconds() -> f64 {
    // `Instant` has no epoch; tdlib's monotonic clock is also an
    // implementation-defined epoch, so 0 + elapsed is the faithful shape.
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

/// `td::Time::now_unadjusted` — the raw monotonic clock.
pub fn now_unadjusted() -> f64 {
    monotonic_seconds()
}

/// `td::Time::now` — monotonic seconds, adjusted to stay non-negative.
pub fn now() -> f64 {
    let diff = f64::from_bits(TIME_DIFF.load(Ordering::Relaxed));
    let mut result = monotonic_seconds() + diff;
    while result < 0.0 {
        let old = f64::from_bits(TIME_DIFF.load(Ordering::Relaxed));
        TIME_DIFF
            .compare_exchange(
                old.to_bits(),
                (old - result).to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();
        let diff = f64::from_bits(TIME_DIFF.load(Ordering::Relaxed));
        result = monotonic_seconds() + diff;
    }
    result
}

/// `td::Time::now_cached` — in tdlib this is currently just `now()`.
pub fn now_cached() -> f64 {
    now()
}

/// `td::Time::jump_in_future(at)` — advances the adjusted clock so that
/// `now() >= at`.
pub fn jump_in_future(at: f64) {
    loop {
        let old = f64::from_bits(TIME_DIFF.load(Ordering::Relaxed));
        let diff = at - now();
        if diff < 0.0 {
            return;
        }
        if TIME_DIFF
            .compare_exchange(
                old.to_bits(),
                (old + diff).to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return;
        }
    }
}

/// `td::relax_timeout_at(timeout, new_timeout)`.
pub fn relax_timeout_at(timeout: &mut f64, new_timeout: f64) {
    if new_timeout == 0.0 {
        return;
    }
    if *timeout == 0.0 || new_timeout < *timeout {
        *timeout = new_timeout;
    }
}

/// `td::Timestamp` — an absolute point on the adjusted monotonic clock;
/// `never()` is the zero (disabled) timestamp.
#[derive(Clone, Copy, Debug)]
pub struct Timestamp {
    at: f64,
}

impl Timestamp {
    /// `td::Timestamp::never()`.
    pub fn never() -> Self {
        Self { at: 0.0 }
    }

    /// `td::Timestamp::now()`.
    pub fn now() -> Self {
        Self { at: now() }
    }

    /// `td::Timestamp::now_cached()`.
    pub fn now_cached() -> Self {
        Self { at: now_cached() }
    }

    /// `td::Timestamp::at_time(timeout)` — named `at_time` because the
    /// C++ overloads `at` with the `at()` accessor.
    pub fn at_time(timeout: f64) -> Self {
        Self { at: timeout }
    }

    /// `td::Timestamp::in(timeout, now)`.
    pub fn in_(timeout: f64, now: Timestamp) -> Self {
        Self {
            at: now.at() + timeout,
        }
    }

    /// `td::Timestamp::is_in_past(now)`.
    pub fn is_in_past_at(&self, now: Timestamp) -> bool {
        self.at <= now.at()
    }

    /// `td::Timestamp::is_in_past()`.
    pub fn is_in_past(&self) -> bool {
        self.is_in_past_at(Self::now_cached())
    }

    /// `td::Timestamp::at_time()`.
    pub fn at(&self) -> f64 {
        self.at
    }

    /// `td::Timestamp::in()` — the time until this timestamp.
    pub fn in_seconds(&self) -> f64 {
        self.at - now_cached()
    }

    /// `td::Timestamp::relax(timeout)` — takes the earlier of the two
    /// enabled timestamps.
    pub fn relax(&mut self, timeout: &Timestamp) {
        if !bool::from(*timeout) {
            return;
        }
        if !bool::from(*self) || self.at > timeout.at {
            self.at = timeout.at;
        }
    }
}

impl From<Timestamp> for bool {
    /// `td::Timestamp::operator bool` — enabled iff `at > 0`.
    fn from(timestamp: Timestamp) -> bool {
        timestamp.at > 0.0
    }
}

impl PartialOrd for Timestamp {
    /// `td::operator<(Timestamp, Timestamp)`.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.at.partial_cmp(&other.at)
    }
}

/// The C++ `operator==` for timestamps compares within `1e-6`.
impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        (self.at - other.at).abs() < 1e-6
    }
}

/// `td::Timer` — elapsed-time stopwatch with pause/resume.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timer {
    elapsed: f64,
    start_time: f64,
    is_paused: bool,
}

impl Timer {
    pub fn new() -> Self {
        Self::new_paused(false)
    }

    /// `td::Timer(is_paused)`.
    pub fn new_paused(is_paused: bool) -> Self {
        Self {
            elapsed: 0.0,
            start_time: now(),
            is_paused,
        }
    }

    /// `td::Timer::elapsed()`.
    pub fn elapsed(&self) -> f64 {
        self.elapsed
            + if self.is_paused {
                0.0
            } else {
                now() - self.start_time
            }
    }

    /// `td::Timer::pause()`.
    pub fn pause(&mut self) {
        if self.is_paused {
            return;
        }
        self.elapsed += now() - self.start_time;
        self.is_paused = true;
    }

    /// `td::Timer::resume()`.
    pub fn resume(&mut self) {
        if !self.is_paused {
            return;
        }
        self.start_time = now();
        self.is_paused = false;
    }
}

/// Sleeps for `duration` on the tokio runtime (`td::usleep_for`-style
/// blocking waits are built on top of this where a runtime exists).
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// `td::Alarm`-style one-shot wakeup: cancels a previous deadline and
/// resolves once the new one passes.
pub struct Alarm {
    inner: Option<tokio::time::Sleep>,
}

impl Alarm {
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// Arms the alarm to fire after `duration`, cancelling the previous one.
    pub fn set(&mut self, duration: Duration) {
        self.inner = Some(tokio::time::sleep(duration));
    }

    /// Cancels the pending deadline, if any.
    pub fn cancel(&mut self) {
        self.inner = None;
    }

    /// Waits until the armed deadline passes (or forever when cancelled).
    pub async fn wait(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.await;
        } else {
            std::future::pending::<()>().await;
        }
    }
}

impl Default for Alarm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_monotonic_and_non_negative() {
        let a = now();
        std::thread::sleep(Duration::from_millis(5));
        let b = now();
        assert!(b > a, "now() advances");
        assert!(a >= 0.0);
        assert_eq!(
            now_cached(),
            now_cached(),
            "cached clock is stable per call"
        );
    }

    #[test]
    fn timestamps_compare_and_relax() {
        let base = Timestamp::at_time(10.0);
        let later = Timestamp::in_(5.0, base);
        assert_eq!(later.at(), 15.0);
        assert!(base.is_in_past_at(later));
        assert!(later.at() > base.at());

        let never = Timestamp::never();
        assert!(!bool::from(never));
        let mut t = Timestamp::at_time(10.0);
        t.relax(&Timestamp::at_time(5.0));
        assert_eq!(t.at(), 5.0);
        t.relax(&never);
        assert_eq!(t.at(), 5.0, "relax with never is a no-op");
        t.relax(&Timestamp::at_time(7.0));
        assert_eq!(t.at(), 5.0, "relax keeps the earlier deadline");

        assert_eq!(
            Timestamp::at_time(1.0),
            Timestamp::at_time(1.0 + 1e-9),
            "1e-6 tolerance"
        );
        assert_ne!(Timestamp::at_time(1.0), Timestamp::at_time(2.0));
    }

    #[test]
    fn jump_in_future_advances_the_clock() {
        let target = now() + 100.0;
        jump_in_future(target);
        assert!(now() >= target);
    }

    #[test]
    fn relax_timeout_at_picks_the_smaller() {
        let mut timeout = 0.0;
        relax_timeout_at(&mut timeout, 5.0);
        assert_eq!(timeout, 5.0);
        relax_timeout_at(&mut timeout, 3.0);
        assert_eq!(timeout, 3.0);
        relax_timeout_at(&mut timeout, 9.0);
        assert_eq!(timeout, 3.0);
        relax_timeout_at(&mut timeout, 0.0);
        assert_eq!(timeout, 3.0, "zero is a no-op");
    }

    #[test]
    fn timer_measures_elapsed() {
        let mut timer = Timer::new();
        std::thread::sleep(Duration::from_millis(20));
        let elapsed = timer.elapsed();
        assert!(elapsed >= 0.015, "elapsed = {elapsed}");

        timer.pause();
        let paused_at = timer.elapsed();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(timer.elapsed(), paused_at, "paused timer does not advance");

        timer.resume();
        std::thread::sleep(Duration::from_millis(20));
        assert!(timer.elapsed() > paused_at, "resumed timer advances");
    }

    #[tokio::test]
    async fn sleep_and_alarm_fire() {
        let start = std::time::Instant::now();
        sleep(Duration::from_millis(30)).await;
        assert!(start.elapsed() >= Duration::from_millis(30));

        let mut alarm = Alarm::new();
        alarm.set(Duration::from_millis(30));
        let start = std::time::Instant::now();
        alarm.wait().await;
        assert!(start.elapsed() >= Duration::from_millis(30));

        alarm.set(Duration::from_millis(100));
        alarm.set(Duration::from_millis(20));
        let start = std::time::Instant::now();
        alarm.wait().await;
        assert!(
            start.elapsed() < Duration::from_millis(80),
            "re-armed alarm wins"
        );
    }
}
