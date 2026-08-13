//! Pure recv-error backoff calculation, factored out of the Windows-only
//! capture loop ([`crate::win`]) so it stays unit-testable on any host.

use std::time::Duration;

/// Backoff duration to sleep after the `consecutive_errors`-th failure in a
/// row: linear in the failure count, clamped to `cap`. A single transient
/// failure costs one `base`-length nap; a handle stuck in a permanently
/// failing state (adapter removed, driver unloaded mid-session) settles at
/// `cap` instead of spinning the thread at 100% CPU on a bare `continue`.
pub fn recv_error_backoff(consecutive_errors: u32, base: Duration, cap: Duration) -> Duration {
    base.saturating_mul(consecutive_errors).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(20);
    const CAP: Duration = Duration::from_millis(500);

    #[test]
    fn zero_consecutive_errors_backs_off_zero() {
        assert_eq!(recv_error_backoff(0, BASE, CAP), Duration::from_secs(0));
    }

    #[test]
    fn first_failure_backs_off_by_one_base_unit() {
        assert_eq!(recv_error_backoff(1, BASE, CAP), BASE);
    }

    #[test]
    fn backoff_grows_linearly_with_consecutive_failures() {
        assert_eq!(recv_error_backoff(5, BASE, CAP), Duration::from_millis(100));
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(recv_error_backoff(1000, BASE, CAP), CAP);
    }
}
