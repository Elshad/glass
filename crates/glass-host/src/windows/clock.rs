use std::sync::OnceLock;
use std::time::Instant;

/// Microseconds on a monotonic clock (QueryPerformanceCounter underneath). Capture
/// timestamps and clock-sync pongs both use this, so latency measurement only needs them
/// to agree with each other.
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// The local date and time as "2026-11-01 15-04", for file names.
pub fn local_stamp() -> String {
    // SAFETY: plain query.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
}
