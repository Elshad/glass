use std::sync::OnceLock;
use std::time::Instant;

/// Microseconds on a monotonic clock (QueryPerformanceCounter underneath). Capture
/// timestamps and clock-sync pongs both use this, so latency measurement only needs them
/// to agree with each other.
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
