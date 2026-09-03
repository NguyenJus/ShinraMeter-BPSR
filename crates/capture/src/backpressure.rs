//! A bounded protocol-event send that never blocks the capture thread.
//!
//! Pipeline-robustness audit, finding 2: `recv_loop` ([`crate::win`]) used a
//! blocking `Sender::send` on a capacity-4096 channel. If the pipeline
//! thread stalls, that block backs the kernel WinDivert queue up behind it,
//! and packets are lost silently — no log line, nothing the overlay can
//! show. [`DropCounter::try_send`] replaces the blocking `send` with
//! `try_send`: on `Full` it drops the event and folds it into a running
//! count instead of stalling, so loss becomes an observable (rate-limited)
//! WARN rather than a silent kernel-side drop.
//!
//! Kept host-testable the same way [`crate::throughput`] and
//! [`crate::backoff`] are: pure bookkeeping over a caller-supplied `now`,
//! with [`crate::win`] owning the actual `log::` call.

use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};

/// How often a sustained stall's drop count is allowed to reach the log —
/// long enough that a wedge does not spam the log at packet rate, short
/// enough that the loss is visible well inside the stall that caused it.
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);

/// What [`DropCounter::try_send`] learned from one send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The event reached the channel.
    Sent,
    /// The channel was full and the event was dropped. `Some(total)` when
    /// this drop is due to be logged now — the first drop ever, or the
    /// first one at least [`LOG_INTERVAL`] after the last log — in which
    /// case `total` is every drop folded in since the last log (including
    /// this one) and the counter has already reset for the next window.
    /// `None` when a drop is still within its window and should stay
    /// silent.
    Dropped(Option<u64>),
    /// The receiving end is gone; the caller should stop sending.
    Disconnected,
}

/// Tracks events dropped by a full channel and decides when that count is
/// due to be logged. One instance per capture loop invocation.
#[derive(Debug, Default)]
pub struct DropCounter {
    dropped_since_log: u64,
    last_logged_at: Option<Instant>,
}

impl DropCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends `event` on `tx` without ever blocking. See the module doc for
    /// why: a stalled pipeline must not stall the capture thread behind it.
    pub fn try_send<T>(&mut self, tx: &Sender<T>, event: T, now: Instant) -> SendOutcome {
        match tx.try_send(event) {
            Ok(()) => SendOutcome::Sent,
            Err(TrySendError::Full(_)) => SendOutcome::Dropped(self.record_drop(now)),
            Err(TrySendError::Disconnected(_)) => SendOutcome::Disconnected,
        }
    }

    /// Folds one drop into the running count and decides whether it is due
    /// to be logged, resetting the count for the next window when it is.
    fn record_drop(&mut self, now: Instant) -> Option<u64> {
        self.dropped_since_log += 1;
        let due = match self.last_logged_at {
            None => true,
            Some(last) => now.duration_since(last) >= LOG_INTERVAL,
        };
        if !due {
            return None;
        }
        let total = self.dropped_since_log;
        self.dropped_since_log = 0;
        self.last_logged_at = Some(now);
        Some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn a_send_with_room_succeeds_and_counts_nothing() {
        let (tx, rx) = bounded::<u32>(1);
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert_eq!(counter.try_send(&tx, 1, now), SendOutcome::Sent);
        assert_eq!(rx.try_recv(), Ok(1));
    }

    #[test]
    fn the_first_drop_on_a_full_channel_is_due_immediately() {
        let (tx, _rx) = bounded::<u32>(1);
        tx.try_send(0).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert_eq!(counter.try_send(&tx, 1, now), SendOutcome::Dropped(Some(1)));
    }

    #[test]
    fn drops_inside_the_log_interval_accumulate_silently() {
        let (tx, _rx) = bounded::<u32>(1);
        tx.try_send(0).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert_eq!(counter.try_send(&tx, 1, now), SendOutcome::Dropped(Some(1)));
        assert_eq!(
            counter.try_send(&tx, 2, now + Duration::from_secs(1)),
            SendOutcome::Dropped(None)
        );
        assert_eq!(
            counter.try_send(&tx, 3, now + Duration::from_millis(4_999)),
            SendOutcome::Dropped(None)
        );
    }

    #[test]
    fn a_drop_at_the_interval_boundary_logs_the_whole_window_and_resets() {
        let (tx, _rx) = bounded::<u32>(1);
        tx.try_send(0).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert_eq!(counter.try_send(&tx, 1, now), SendOutcome::Dropped(Some(1)));
        assert_eq!(
            counter.try_send(&tx, 2, now + Duration::from_secs(2)),
            SendOutcome::Dropped(None)
        );
        assert_eq!(
            counter.try_send(&tx, 3, now + LOG_INTERVAL),
            SendOutcome::Dropped(Some(2)),
            "the boundary drop must report every drop folded in since the last log \
             (drops 2 and 3 — drop 1 was already logged and reset the window)"
        );

        // The window reset, so the very next drop starts a fresh count
        // rather than being due immediately again.
        assert_eq!(
            counter.try_send(&tx, 4, now + LOG_INTERVAL + Duration::from_millis(1)),
            SendOutcome::Dropped(None)
        );
    }

    #[test]
    fn a_disconnected_receiver_is_reported_and_never_counted_as_a_drop() {
        let (tx, rx) = bounded::<u32>(1);
        drop(rx);
        let mut counter = DropCounter::new();

        assert_eq!(
            counter.try_send(&tx, 1, Instant::now()),
            SendOutcome::Disconnected
        );
    }
}
