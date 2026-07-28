//! Monotonic time on clock_gettime, replacing std::time::Instant.
//!
//! The launcher only needs elapsed durations for the caret blink, key repeat,
//! and double-click detection, so this is a thin wrapper over CLOCK_MONOTONIC
//! rather than a full Duration/Instant arithmetic surface.

use crate::platform::syscall::{self, CLOCK_MONOTONIC};

/// Read the monotonic clock in nanoseconds since an arbitrary epoch.
fn now_nanos() -> u64 {
    let ts = syscall::clock_gettime(CLOCK_MONOTONIC);
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

/// A point on the monotonic clock, like std::time::Instant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    /// The current monotonic time.
    pub fn now() -> Self {
        Instant(now_nanos())
    }

    /// Milliseconds elapsed since this instant (saturating at zero).
    pub fn elapsed_ms(&self) -> u64 {
        now_nanos().saturating_sub(self.0) / 1_000_000
    }

    /// Milliseconds from earlier to this instant (saturating at zero).
    pub fn ms_since(&self, earlier: Instant) -> u64 {
        self.0.saturating_sub(earlier.0) / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_advances_monotonically() {
        let start = Instant::now();
        // Busy-spin briefly; the monotonic clock must not go backwards.
        let mut spins = 0u64;
        while start.elapsed_ms() == 0 && spins < 50_000_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        assert!(
            start.elapsed_ms() < 60_000,
            "elapsed should be a small value"
        );
        assert!(Instant::now() >= start);
    }
}
