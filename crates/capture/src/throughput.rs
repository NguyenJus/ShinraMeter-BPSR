//! What capture is actually delivering, accounted cheaply enough to log.
//!
//! Issue #213: during the #211 wedge the process was alive, packets kept
//! arriving, and *zero* bytes reached the decoder for 24 minutes — and the
//! log said nothing at all, so "the game stopped sending" and "reassembly
//! ate the stream" were indistinguishable after the fact. A single
//! "0 bytes delivered in 60s" line would have named it immediately.
//!
//! Pure bookkeeping over a caller-supplied `now`, so the rate limiting and
//! the stall verdict are host-testable without sleeping or a real clock.
//! [`crate::win`] owns the `log::` calls and the recovery action; this
//! module only decides *when* there is something to say.

use std::time::{Duration, Instant};

/// How often a throughput line is emitted. Long enough that a busy capture
/// adds one line a minute to a log a user might have to hand over, short
/// enough that a wedge is visible well inside a single raid.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// How long the decoder may receive nothing, while packets keep arriving on
/// the adopted connection, before capture re-anchors itself (#214).
///
/// Generous on purpose: a legitimate lull between pulls is seconds, not
/// minutes, but a self-restart drops the decoder's state, so the cost of
/// firing early is real while the cost of firing late is three minutes of a
/// wedge the user would otherwise have had to sit out entirely.
pub const STALL_RESTART_AFTER: Duration = Duration::from_secs(180);

/// One window's worth of throughput, handed back by
/// [`ThroughputMonitor::poll`] for the caller to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    /// Bytes handed to the decoder during the window.
    pub bytes: u64,
    /// Payload-carrying segments pushed into reassembly during the window.
    pub packets: u64,
    /// How long the window actually covered — the configured interval plus
    /// however long the packet that closed it took to arrive.
    pub window: Duration,
    /// How long it has been since any byte reached the decoder, measured
    /// from the monitor's start if none ever has. Distinguishes "quiet
    /// right now" from "dead since startup".
    pub silent_for: Duration,
}

impl Heartbeat {
    /// Whether nothing at all reached the decoder this window — the #211
    /// signature when [`Self::packets`] is nonetheless nonzero.
    pub fn is_silent(&self) -> bool {
        self.bytes == 0
    }
}

/// Rate-limited accounting of bytes reaching the decoder, plus the verdict
/// on whether capture has wedged badly enough to be worth restarting.
pub struct ThroughputMonitor {
    interval: Duration,
    stall_after: Duration,
    /// Start of the window [`Self::poll`] is currently accumulating.
    window_start: Instant,
    /// Bytes and packets seen since `window_start`.
    window_bytes: u64,
    window_packets: u64,
    /// When a byte last reached the decoder, seeded with the monitor's
    /// start so a capture that never delivers anything still ages.
    last_delivery: Instant,
    /// Packets pushed into reassembly since `last_delivery`. Gates the
    /// restart verdict: with no packets at all there is no wedged stream to
    /// re-anchor — the game is simply not sending — and restarting would be
    /// pure log noise.
    packets_since_delivery: u64,
}

impl ThroughputMonitor {
    /// A monitor with the shipped [`HEARTBEAT_INTERVAL`] and
    /// [`STALL_RESTART_AFTER`], starting its first window at `start`.
    pub fn new(start: Instant) -> Self {
        Self::with_limits(start, HEARTBEAT_INTERVAL, STALL_RESTART_AFTER)
    }

    pub fn with_limits(start: Instant, interval: Duration, stall_after: Duration) -> Self {
        Self {
            interval,
            stall_after,
            window_start: start,
            window_bytes: 0,
            window_packets: 0,
            last_delivery: start,
            packets_since_delivery: 0,
        }
    }

    /// Records one payload-carrying segment offered to reassembly.
    pub fn record_packet(&mut self) {
        self.window_packets = self.window_packets.saturating_add(1);
        self.packets_since_delivery = self.packets_since_delivery.saturating_add(1);
    }

    /// Records `bytes` handed to the decoder, which also clears the stall
    /// timer: the stream is demonstrably flowing again.
    pub fn record_delivered(&mut self, bytes: usize, now: Instant) {
        self.window_bytes = self.window_bytes.saturating_add(bytes as u64);
        self.last_delivery = now;
        self.packets_since_delivery = 0;
    }

    /// Closes and returns the current window if the interval has elapsed,
    /// starting a fresh one; `None` otherwise. Called on every packet, so
    /// the rate limit — not the call site — is what keeps the log quiet.
    pub fn poll(&mut self, now: Instant) -> Option<Heartbeat> {
        let window = now.saturating_duration_since(self.window_start);
        if window < self.interval {
            return None;
        }
        let beat = Heartbeat {
            bytes: self.window_bytes,
            packets: self.window_packets,
            window,
            silent_for: now.saturating_duration_since(self.last_delivery),
        };
        self.window_start = now;
        self.window_bytes = 0;
        self.window_packets = 0;
        Some(beat)
    }

    /// Whether capture should re-anchor itself now (#214): packets are
    /// arriving on the adopted connection but nothing has reached the
    /// decoder for [`Self::stall_after`].
    ///
    /// Firing resets the stall timer, so one wedge asks for one restart
    /// rather than one per packet — and, if that restart does not take, the
    /// next attempt comes a full window later rather than never.
    pub fn restart_due(&mut self, now: Instant) -> bool {
        if self.packets_since_delivery == 0 {
            return false;
        }
        if now.saturating_duration_since(self.last_delivery) < self.stall_after {
            return false;
        }
        self.last_delivery = now;
        self.packets_since_delivery = 0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const INTERVAL: Duration = Duration::from_secs(60);
    const STALL_AFTER: Duration = Duration::from_secs(180);

    fn monitor(start: Instant) -> ThroughputMonitor {
        ThroughputMonitor::with_limits(start, INTERVAL, STALL_AFTER)
    }

    #[test]
    fn no_heartbeat_before_the_interval_elapses() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        m.record_delivered(100, start + Duration::from_secs(1));
        assert_eq!(m.poll(start + Duration::from_secs(59)), None);
    }

    #[test]
    fn a_heartbeat_reports_the_traffic_of_the_window() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        m.record_packet();
        m.record_delivered(1_000, start + Duration::from_secs(5));

        let beat = m.poll(start + INTERVAL).expect("the interval elapsed");
        assert_eq!(beat.bytes, 1_000);
        assert_eq!(beat.packets, 2);
        assert_eq!(beat.window, INTERVAL);
        assert!(!beat.is_silent());
    }

    #[test]
    fn the_window_resets_after_a_heartbeat() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        m.record_delivered(10, start);
        m.poll(start + INTERVAL).expect("first window");

        m.record_packet();
        m.record_delivered(7, start + INTERVAL + Duration::from_secs(1));
        let beat = m.poll(start + INTERVAL * 2).expect("second window");
        assert_eq!(
            beat.bytes, 7,
            "the first window's bytes must not carry over"
        );
        assert_eq!(beat.packets, 1);
    }

    /// The #211 signature: the process is alive and packets keep arriving,
    /// but nothing reaches the decoder. That has to be visible as a
    /// distinct, positively-logged fact rather than as log silence.
    #[test]
    fn a_window_with_no_delivered_bytes_is_silent() {
        let start = Instant::now();
        let mut m = monitor(start);
        for _ in 0..500 {
            m.record_packet();
        }

        let beat = m.poll(start + INTERVAL).expect("the interval elapsed");
        assert_eq!(beat.bytes, 0);
        assert_eq!(beat.packets, 500);
        assert!(beat.is_silent(), "zero delivered bytes is the whole point");
        assert!(beat.silent_for >= INTERVAL, "{:?}", beat.silent_for);
    }

    #[test]
    fn traffic_that_keeps_flowing_never_asks_for_a_restart() {
        let start = Instant::now();
        let mut m = monitor(start);
        for tick in 1..=20u32 {
            let now = start + Duration::from_secs(30 * u64::from(tick));
            m.record_packet();
            m.record_delivered(1_400, now);
            assert!(!m.restart_due(now), "tick {tick}");
        }
    }

    #[test]
    fn packets_with_nothing_delivered_ask_for_a_restart_after_the_stall_window() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        assert!(!m.restart_due(start + STALL_AFTER - Duration::from_secs(1)));
        assert!(m.restart_due(start + STALL_AFTER));
    }

    /// A closed game (or a link with nothing on it) is not a wedge — there
    /// is no stream to re-anchor, so restarting capture would be pure noise.
    #[test]
    fn an_idle_link_never_asks_for_a_restart() {
        let start = Instant::now();
        let mut m = monitor(start);
        assert!(!m.restart_due(start + STALL_AFTER * 10));
    }

    #[test]
    fn a_restart_is_not_asked_for_again_until_another_stall_window_passes() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        assert!(m.restart_due(start + STALL_AFTER));

        m.record_packet();
        assert!(
            !m.restart_due(start + STALL_AFTER + Duration::from_secs(1)),
            "one wedge must not spawn a restart on every packet"
        );
        assert!(
            m.restart_due(start + STALL_AFTER * 2),
            "a still-wedged capture must be retried"
        );
    }

    /// Delivery after a stall clears it: the restart timer is measured from
    /// the last byte that actually reached the decoder, not from startup.
    #[test]
    fn a_delivery_clears_the_stall_timer() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet();
        let recovered = start + STALL_AFTER - Duration::from_secs(1);
        m.record_delivered(1_400, recovered);
        assert!(
            !m.restart_due(start + STALL_AFTER),
            "the original stall window must not still fire after a delivery"
        );

        // Wedged again from `recovered` on: packets keep arriving, nothing
        // is delivered. The next restart is due a full window after the
        // delivery, not a full window after startup.
        m.record_packet();
        assert!(m.restart_due(recovered + STALL_AFTER));
    }
}
