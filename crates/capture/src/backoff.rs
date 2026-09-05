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

/// Next game-pid lookup interval (issue #337, O6), given the `current` one:
/// doubles it, clamped to `cap`. [`crate::win`]'s capture loop re-runs a
/// whole-system `Toolhelp` snapshot every time this interval elapses while
/// the game's pid is still unresolved; a game that never launches (or has
/// already exited) would otherwise pay that cost every couple of seconds
/// forever. Doubling backs that off quickly while still resolving promptly
/// right after the game actually starts; the caller resets `current` back
/// to the initial interval on a restart or a tracked-connection teardown; see
/// `win.rs`'s `recv_loop` for both reset sites.
pub fn next_game_pid_lookup_interval(current: Duration, cap: Duration) -> Duration {
    current.saturating_mul(2).min(cap)
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

    const LOOKUP_INITIAL: Duration = Duration::from_secs(2);
    const LOOKUP_CAP: Duration = Duration::from_secs(60);

    #[test]
    fn game_pid_lookup_interval_doubles_each_time() {
        let mut interval = LOOKUP_INITIAL;
        let mut seen = vec![interval];
        for _ in 0..5 {
            interval = next_game_pid_lookup_interval(interval, LOOKUP_CAP);
            seen.push(interval);
        }
        assert_eq!(
            seen,
            vec![2, 4, 8, 16, 32, 60]
                .into_iter()
                .map(Duration::from_secs)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn game_pid_lookup_interval_clamps_to_the_cap() {
        assert_eq!(
            next_game_pid_lookup_interval(LOOKUP_CAP, LOOKUP_CAP),
            LOOKUP_CAP
        );
        assert_eq!(
            next_game_pid_lookup_interval(Duration::from_secs(45), LOOKUP_CAP),
            LOOKUP_CAP
        );
    }
}
