use nix::time::{ClockId, clock_gettime};

/// Returns Linux `CLOCK_BOOTTIME` in milliseconds.
///
/// Boot time, not wall time: a clock step or a suspend must never make stale
/// presence look fresh.
///
/// # Panics
///
/// Panics only if the Linux kernel does not provide `CLOCK_BOOTTIME`; this is a
/// hard platform requirement and the daemon must not authorize stale state instead.
#[must_use]
pub fn boottime_ms() -> u64 {
    let time =
        clock_gettime(ClockId::CLOCK_BOOTTIME).expect("CLOCK_BOOTTIME is available on Linux");
    u64::try_from(time.tv_sec())
        .unwrap_or(0)
        .saturating_mul(1_000)
        .saturating_add(u64::try_from(time.tv_nsec()).unwrap_or(0) / 1_000_000)
}
