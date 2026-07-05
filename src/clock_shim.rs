//! Real monotonic clock adapter (EXCLUDED from coverage).
//!
//! [`SystemClock`] is the production implementation of the [`Clock`] port used
//! by the debouncer. It reads the wall/monotonic clock via
//! [`std::time::Instant`], an OS time source whose values cannot be
//! meaningfully asserted in a unit test, so it lives behind the port here. All
//! debounce *decision* logic lives in [`crate::daemon`] and is tested with a
//! hand-advanced fake clock.

use crate::daemon::Clock;

/// [`Clock`] backed by [`std::time::Instant`].
pub struct SystemClock {
    start: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}
