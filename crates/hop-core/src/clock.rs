//! One monotonic microsecond clock for the whole process.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the first call. Never 0, so 0 can mean "unset" on the wire.
pub fn now_us() -> u64 {
    let start = *START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64 + 1
}
