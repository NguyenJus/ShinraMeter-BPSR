//! What capture is actually delivering, accounted cheaply enough to log.
//!
//! Issue #213: during the #211 wedge the process was alive, packets kept
//! arriving, and *zero* bytes reached the decoder for 24 minutes — and the
//! log said nothing at all, so "the game stopped sending" and "reassembly
//! ate the stream" were indistinguishable after the fact. A single
//! "0 bytes delivered in 60s" line would have named it immediately.
//!
//! Issue #271: the first cut of that only ever ran from the bottom of the
//! WinDivert packet loop, below six `continue`s — two of which skip every
//! packet that is not a server→client segment of the currently adopted
//! flow. So the heartbeat (and #214's self-restart, gated behind the same
//! point) went quiet in exactly the situations they were built to name.
//! The accounting therefore lives behind a [`SharedMonitor`]: the capture
//! thread only *records*, and a separate ticker ([`run_watchdog`]) decides
//! on a wall clock, so nothing about the fix depends on a packet reaching
//! the bottom of that loop — or on a packet arriving at all.
//!
//! Pure bookkeeping over a caller-supplied `now`, so the rate limiting and
//! the stall verdict are host-testable without sleeping or a real clock.
//! [`crate::win`] owns the `log::` calls and the recovery action; this
//! module only decides *when* there is something to say.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
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

/// How often the watchdog thread wakes to ask [`SharedMonitor::tick`]
/// whether anything is due. Far shorter than [`HEARTBEAT_INTERVAL`] — the
/// interval, not the wakeup rate, is what keeps the log quiet — and short
/// enough that the thread notices the stop flag promptly at shutdown.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// What a finished window says about capture, which is the whole point of
/// #213: four outcomes a log reader must be able to tell apart, three of
/// which used to produce identical silence (#271).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatKind {
    /// Bytes reached the decoder. Capture is working.
    Delivering,
    /// Payload packets arrived on the adopted connection and *none* of
    /// their bytes reached the decoder — the #211 fingerprint, and the only
    /// case #214's self-restart can help.
    Wedged,
    /// Packets crossed the link, but none of them belonged to an adopted
    /// game connection. The capture handle is demonstrably alive and the
    /// game is not sending: "app up, game closed", or detection has not
    /// adopted anything yet. Nothing to re-anchor.
    NoGameTraffic,
    /// The driver handed the loop no packets at all this window. On a
    /// filter as wide as `!loopback && ip && tcp`, that is either a machine
    /// with no network traffic whatsoever or a capture handle that has
    /// stopped delivering — which is worth saying out loud, because it is
    /// the one outcome the user cannot diagnose from the app's UI.
    LinkSilent,
}

/// One window's worth of throughput, handed back by
/// [`ThroughputMonitor::poll`] for the caller to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    /// Bytes handed to the decoder during the window.
    pub bytes: u64,
    /// Payload-carrying segments pushed into reassembly during the window —
    /// i.e. server→client packets of the *adopted* connection only.
    pub packets: u64,
    /// Every packet the driver handed the capture loop during the window,
    /// counted before any classification. Separates "nothing arrived" from
    /// "plenty arrived, none of it the game's" (#271).
    pub observed: u64,
    /// How long the window actually covered — the configured interval plus
    /// however long the tick that closed it took to come round.
    pub window: Duration,
    /// How long it has been since any byte reached the decoder, measured
    /// from the monitor's start if none ever has. Distinguishes "quiet
    /// right now" from "dead since startup".
    pub silent_for: Duration,
    /// Whether capture currently has a game connection adopted.
    pub adopted: bool,
    /// Reassembly's out-of-order cache at the close of the window.
    pub gap_segments: usize,
    pub gap_bytes: usize,
}

impl Heartbeat {
    /// Whether nothing at all reached the decoder this window — the #211
    /// signature when [`Self::packets`] is nonetheless nonzero.
    pub fn is_silent(&self) -> bool {
        self.bytes == 0
    }

    /// Which of the four outcomes this window was. The classification lives
    /// here rather than at the (Windows-only, untestable) log call site so
    /// that "no packets at all" versus "packets, but none on the adopted
    /// flow" is a decision a host test can pin down.
    pub fn kind(&self) -> HeartbeatKind {
        if self.bytes > 0 {
            HeartbeatKind::Delivering
        } else if self.packets > 0 {
            HeartbeatKind::Wedged
        } else if self.observed > 0 {
            HeartbeatKind::NoGameTraffic
        } else {
            HeartbeatKind::LinkSilent
        }
    }
}

/// What one watchdog wakeup found due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    /// A closed window to log, if the interval elapsed.
    pub beat: Option<Heartbeat>,
    /// Whether capture should re-anchor itself now (#214).
    pub restart: bool,
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
    window_observed: u64,
    /// When a byte last reached the decoder, seeded with the monitor's
    /// start so a capture that never delivers anything still ages.
    last_delivery: Instant,
    /// Packets pushed into reassembly since `last_delivery`. Gates the
    /// restart verdict: with no packets at all there is no wedged stream to
    /// re-anchor — the game is simply not sending — and restarting would be
    /// pure log noise.
    packets_since_delivery: u64,
    /// When the last adopted-flow payload packet arrived. `None` before the
    /// first one and after the tracked connection goes away. The restart
    /// verdict requires this to be *recent*: #214's message claims packets
    /// "kept arriving", and now that the verdict is evaluated on a wall
    /// clock rather than only when a packet happens to reach the bottom of
    /// the loop (#271), that claim has to be checked rather than assumed.
    last_packet: Option<Instant>,
    /// Whether a game connection is currently adopted, for the log line.
    adopted: bool,
    /// Reassembly's out-of-order cache, republished by the capture thread
    /// so the watchdog can report it without touching the reassembler.
    gap_segments: usize,
    gap_bytes: usize,
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
            window_observed: 0,
            last_delivery: start,
            packets_since_delivery: 0,
            last_packet: None,
            adopted: false,
            gap_segments: 0,
            gap_bytes: 0,
        }
    }

    /// Records one packet handed to the capture loop by the driver, counted
    /// before any classification so a window can distinguish "the handle
    /// delivered nothing" from "the handle delivered plenty, none of it the
    /// game's".
    pub fn record_observed(&mut self) {
        self.window_observed = self.window_observed.saturating_add(1);
    }

    /// Records one payload-carrying segment of the adopted connection
    /// offered to reassembly.
    pub fn record_packet(&mut self, now: Instant) {
        self.window_packets = self.window_packets.saturating_add(1);
        self.packets_since_delivery = self.packets_since_delivery.saturating_add(1);
        self.last_packet = Some(now);
    }

    /// Records `bytes` handed to the decoder, which also clears the stall
    /// timer: the stream is demonstrably flowing again.
    pub fn record_delivered(&mut self, bytes: usize, now: Instant) {
        self.window_bytes = self.window_bytes.saturating_add(bytes as u64);
        self.last_delivery = now;
        self.packets_since_delivery = 0;
    }

    /// Publishes reassembly's out-of-order cache for the next heartbeat.
    pub fn record_gap_cache(&mut self, segments: usize, bytes: usize) {
        self.gap_segments = segments;
        self.gap_bytes = bytes;
    }

    /// Notes that capture has adopted a game connection.
    pub fn note_adopted(&mut self) {
        self.adopted = true;
    }

    /// Notes that the tracked connection is gone — torn down, or dropped by
    /// a restart. Clears the stall evidence with it: there is no longer a
    /// wedged stream to re-anchor, so a restart must not fire on the back of
    /// packets that belonged to a connection that no longer exists. Leaves
    /// `last_delivery` alone, so the heartbeat can still say how long it has
    /// been since the last byte.
    pub fn note_detached(&mut self) {
        self.adopted = false;
        self.packets_since_delivery = 0;
        self.last_packet = None;
    }

    /// Closes and returns the current window if the interval has elapsed,
    /// starting a fresh one; `None` otherwise. Driven by the watchdog's
    /// wall clock, so a window closes on schedule whatever the packet loop
    /// is (or is not) seeing.
    pub fn poll(&mut self, now: Instant) -> Option<Heartbeat> {
        let window = now.saturating_duration_since(self.window_start);
        if window < self.interval {
            return None;
        }
        let beat = Heartbeat {
            bytes: self.window_bytes,
            packets: self.window_packets,
            observed: self.window_observed,
            window,
            silent_for: now.saturating_duration_since(self.last_delivery),
            adopted: self.adopted,
            gap_segments: self.gap_segments,
            gap_bytes: self.gap_bytes,
        };
        self.window_start = now;
        self.window_bytes = 0;
        self.window_packets = 0;
        self.window_observed = 0;
        Some(beat)
    }

    /// Whether capture should re-anchor itself now (#214): packets are
    /// arriving on the adopted connection but nothing has reached the
    /// decoder for [`Self::stall_after`].
    ///
    /// "Are arriving", present tense, is load-bearing. Evaluated on a wall
    /// clock the verdict would otherwise fire for a connection that stopped
    /// sending minutes ago — a closed game, not a wedge — so the last
    /// adopted-flow packet must be no older than one heartbeat window.
    ///
    /// Firing resets the stall timer, so one wedge asks for one restart
    /// rather than one per tick — and, if that restart does not take, the
    /// next attempt comes a full window later rather than never.
    pub fn restart_due(&mut self, now: Instant) -> bool {
        if self.packets_since_delivery == 0 {
            return false;
        }
        let Some(last_packet) = self.last_packet else {
            return false;
        };
        if now.saturating_duration_since(last_packet) >= self.interval {
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

/// A [`ThroughputMonitor`] the capture thread writes to and the watchdog
/// thread reads from.
///
/// The lock is taken once or twice per packet on the recording side and
/// once per [`WATCHDOG_TICK`] on the deciding side; it is never held across
/// a `log::` call, a `recv`, or anything else that can block.
#[derive(Clone)]
pub struct SharedMonitor(Arc<Mutex<ThroughputMonitor>>);

impl SharedMonitor {
    pub fn new(start: Instant) -> Self {
        Self(Arc::new(Mutex::new(ThroughputMonitor::new(start))))
    }

    pub fn with_limits(start: Instant, interval: Duration, stall_after: Duration) -> Self {
        Self(Arc::new(Mutex::new(ThroughputMonitor::with_limits(
            start,
            interval,
            stall_after,
        ))))
    }

    /// The guard, recovered from poisoning rather than propagated: a panic
    /// elsewhere must not take the diagnostics down with it, and every
    /// field here is plain counters that stay perfectly readable.
    fn lock(&self) -> MutexGuard<'_, ThroughputMonitor> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn record_observed(&self) {
        self.lock().record_observed();
    }

    pub fn record_packet(&self, now: Instant) {
        self.lock().record_packet(now);
    }

    pub fn record_delivered(&self, bytes: usize, now: Instant) {
        self.lock().record_delivered(bytes, now);
    }

    pub fn record_gap_cache(&self, segments: usize, bytes: usize) {
        self.lock().record_gap_cache(segments, bytes);
    }

    pub fn note_adopted(&self) {
        self.lock().note_adopted();
    }

    pub fn note_detached(&self) {
        self.lock().note_detached();
    }

    /// One wakeup's worth of decisions, taken under a single lock.
    pub fn tick(&self, now: Instant) -> Tick {
        let mut monitor = self.lock();
        Tick {
            beat: monitor.poll(now),
            restart: monitor.restart_due(now),
        }
    }
}

/// The wall clock behind the heartbeat: wakes every `tick_every`, asks
/// `monitor` what is due, and hands it to `on_tick`, until `stop` is set.
///
/// This is the whole of #271's fix. Nothing here can be skipped by a
/// `continue` in the packet loop, and nothing here waits on a packet — so a
/// window in which capture received nothing at all still produces a line,
/// which is what #213 asked for and never got.
pub fn run_watchdog(
    monitor: &SharedMonitor,
    stop: &AtomicBool,
    tick_every: Duration,
    mut on_tick: impl FnMut(Tick),
) {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(tick_every);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        on_tick(monitor.tick(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const INTERVAL: Duration = Duration::from_secs(60);
    const STALL_AFTER: Duration = Duration::from_secs(180);

    fn monitor(start: Instant) -> ThroughputMonitor {
        ThroughputMonitor::with_limits(start, INTERVAL, STALL_AFTER)
    }

    /// A flow that is still live: one adopted-flow packet every `step`
    /// seconds from `start` through `start + until`, inclusive.
    fn keep_packets_arriving(
        m: &mut ThroughputMonitor,
        start: Instant,
        until: Duration,
        step: u64,
    ) {
        let mut elapsed = 0;
        while elapsed <= until.as_secs() {
            m.record_packet(start + Duration::from_secs(elapsed));
            elapsed += step;
        }
    }

    #[test]
    fn no_heartbeat_before_the_interval_elapses() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_packet(start);
        m.record_delivered(100, start + Duration::from_secs(1));
        assert_eq!(m.poll(start + Duration::from_secs(59)), None);
    }

    #[test]
    fn a_heartbeat_reports_the_traffic_of_the_window() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_observed();
        m.record_observed();
        m.record_packet(start);
        m.record_packet(start);
        m.record_delivered(1_000, start + Duration::from_secs(5));
        m.record_gap_cache(2, 900);

        let beat = m.poll(start + INTERVAL).expect("the interval elapsed");
        assert_eq!(beat.bytes, 1_000);
        assert_eq!(beat.packets, 2);
        assert_eq!(beat.observed, 2);
        assert_eq!(beat.window, INTERVAL);
        assert_eq!(beat.gap_segments, 2);
        assert_eq!(beat.gap_bytes, 900);
        assert!(!beat.is_silent());
        assert_eq!(beat.kind(), HeartbeatKind::Delivering);
    }

    #[test]
    fn the_window_resets_after_a_heartbeat() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.record_observed();
        m.record_packet(start);
        m.record_delivered(10, start);
        m.poll(start + INTERVAL).expect("first window");

        m.record_observed();
        m.record_packet(start + INTERVAL);
        m.record_delivered(7, start + INTERVAL + Duration::from_secs(1));
        let beat = m.poll(start + INTERVAL * 2).expect("second window");
        assert_eq!(
            beat.bytes, 7,
            "the first window's bytes must not carry over"
        );
        assert_eq!(beat.packets, 1);
        assert_eq!(beat.observed, 1, "observed packets must reset too");
    }

    /// The #211 signature: the process is alive and packets keep arriving,
    /// but nothing reaches the decoder. That has to be visible as a
    /// distinct, positively-logged fact rather than as log silence.
    #[test]
    fn a_window_with_no_delivered_bytes_is_silent() {
        let start = Instant::now();
        let mut m = monitor(start);
        for _ in 0..500 {
            m.record_observed();
            m.record_packet(start);
        }

        let beat = m.poll(start + INTERVAL).expect("the interval elapsed");
        assert_eq!(beat.bytes, 0);
        assert_eq!(beat.packets, 500);
        assert!(beat.is_silent(), "zero delivered bytes is the whole point");
        assert!(beat.silent_for >= INTERVAL, "{:?}", beat.silent_for);
        assert_eq!(beat.kind(), HeartbeatKind::Wedged);
    }

    /// Issue #271's flagship case: the game is closed, so nothing is ever
    /// adopted, but the link is busy with unrelated TCP. The old code
    /// polled from below the `Unrelated` `continue` and emitted nothing at
    /// all for a whole 18-minute session; the window must close anyway.
    #[test]
    fn a_window_with_no_adopted_flow_packets_still_ticks() {
        let start = Instant::now();
        let mut m = monitor(start);
        for _ in 0..10_000 {
            m.record_observed();
        }

        let beat = m.poll(start + INTERVAL).expect("the interval elapsed");
        assert_eq!(beat.packets, 0, "nothing was on the adopted flow");
        assert_eq!(beat.observed, 10_000, "but the link was far from idle");
        assert!(!beat.adopted);
        assert_eq!(
            beat.kind(),
            HeartbeatKind::NoGameTraffic,
            "a busy link with no game traffic is not a wedge and not a dead handle"
        );
    }

    /// The other half of the same distinction: the driver handed the loop
    /// nothing whatsoever. Same zero bytes, entirely different diagnosis,
    /// and the log has to be able to say which (#271).
    #[test]
    fn no_packets_at_all_is_distinguishable_from_no_game_packets() {
        let start = Instant::now();
        let mut silent = monitor(start);
        let silent_beat = silent.poll(start + INTERVAL).expect("the interval elapsed");

        let mut busy = monitor(start);
        busy.record_observed();
        let busy_beat = busy.poll(start + INTERVAL).expect("the interval elapsed");

        assert_eq!(silent_beat.bytes, busy_beat.bytes, "both delivered nothing");
        assert_eq!(silent_beat.packets, busy_beat.packets, "both saw no game");
        assert_eq!(silent_beat.kind(), HeartbeatKind::LinkSilent);
        assert_eq!(busy_beat.kind(), HeartbeatKind::NoGameTraffic);
        assert_ne!(
            silent_beat.kind(),
            busy_beat.kind(),
            "these are different failures and must not log the same line"
        );
    }

    #[test]
    fn a_heartbeat_reports_whether_a_connection_is_adopted() {
        let start = Instant::now();
        let mut m = monitor(start);
        m.note_adopted();
        assert!(m.poll(start + INTERVAL).expect("first window").adopted);

        m.note_detached();
        assert!(!m.poll(start + INTERVAL * 2).expect("second window").adopted);
    }

    #[test]
    fn traffic_that_keeps_flowing_never_asks_for_a_restart() {
        let start = Instant::now();
        let mut m = monitor(start);
        for tick in 1..=20u32 {
            let now = start + Duration::from_secs(30 * u64::from(tick));
            m.record_packet(now);
            m.record_delivered(1_400, now);
            assert!(!m.restart_due(now), "tick {tick}");
        }
    }

    #[test]
    fn packets_with_nothing_delivered_ask_for_a_restart_after_the_stall_window() {
        let start = Instant::now();
        let mut m = monitor(start);
        keep_packets_arriving(&mut m, start, STALL_AFTER, 10);
        assert!(!m.restart_due(start + STALL_AFTER - Duration::from_secs(1)));
        assert!(m.restart_due(start + STALL_AFTER));
    }

    /// A closed game (or a link with nothing on it) is not a wedge — there
    /// is no stream to re-anchor, so restarting capture would be pure noise.
    /// The verdict is now reached on a wall clock (#271), so this is the
    /// case that has to keep holding on every single tick, forever.
    #[test]
    fn an_idle_link_never_asks_for_a_restart() {
        let start = Instant::now();
        let mut m = monitor(start);
        assert!(!m.restart_due(start + STALL_AFTER * 10));

        let shared = SharedMonitor::with_limits(start, INTERVAL, STALL_AFTER);
        for tick in 1..=100u64 {
            let t = shared.tick(start + Duration::from_secs(30 * tick));
            assert!(!t.restart, "tick {tick} asked to restart an idle capture");
        }
    }

    /// The verdict is now reached on a wall clock rather than only when an
    /// adopted-flow packet happens to arrive (#271), which is exactly the
    /// case that could turn a closed game into a restart loop: the last
    /// packets are old, the stall window passes on its own, and there is
    /// nothing left to re-anchor. Recency of the *packets* is what keeps
    /// #214 pointed at a real wedge.
    #[test]
    fn a_flow_that_went_quiet_is_not_a_wedge() {
        let start = Instant::now();
        let mut m = monitor(start);
        for second in 0..30 {
            m.record_packet(start + Duration::from_secs(second));
        }
        // The game stopped sending at t+30s. Every later tick must decline.
        for minute in 1..=10u64 {
            let now = start + Duration::from_secs(60 * minute);
            assert!(
                !m.restart_due(now),
                "a flow silent since t+30s must not be restarted at t+{}s",
                now.saturating_duration_since(start).as_secs(),
            );
        }
    }

    /// A torn-down connection takes its stall evidence with it, so the
    /// packets it delivered before the FIN cannot fund a restart afterwards.
    #[test]
    fn a_teardown_cancels_a_pending_restart() {
        let start = Instant::now();
        let mut m = monitor(start);
        keep_packets_arriving(&mut m, start, STALL_AFTER, 10);
        m.note_detached();
        assert!(
            !m.restart_due(start + STALL_AFTER),
            "the connection is gone; there is nothing to re-anchor"
        );
    }

    #[test]
    fn a_restart_is_not_asked_for_again_until_another_stall_window_passes() {
        let start = Instant::now();
        let mut m = monitor(start);
        keep_packets_arriving(&mut m, start, STALL_AFTER, 10);
        assert!(m.restart_due(start + STALL_AFTER));

        m.record_packet(start + STALL_AFTER + Duration::from_secs(1));
        assert!(
            !m.restart_due(start + STALL_AFTER + Duration::from_secs(1)),
            "one wedge must not spawn a restart on every tick"
        );
        keep_packets_arriving(&mut m, start + STALL_AFTER, STALL_AFTER, 10);
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
        let recovered = start + STALL_AFTER - Duration::from_secs(1);
        keep_packets_arriving(&mut m, start, STALL_AFTER, 10);
        m.record_delivered(1_400, recovered);
        assert!(
            !m.restart_due(start + STALL_AFTER),
            "the original stall window must not still fire after a delivery"
        );

        // Wedged again from `recovered` on: packets keep arriving, nothing
        // is delivered. The next restart is due a full window after the
        // delivery, not a full window after startup.
        keep_packets_arriving(&mut m, recovered, STALL_AFTER, 10);
        assert!(m.restart_due(recovered + STALL_AFTER));
    }

    #[test]
    fn a_tick_reports_the_window_and_the_restart_verdict_together() {
        let start = Instant::now();
        let shared = SharedMonitor::with_limits(start, INTERVAL, STALL_AFTER);
        shared.record_observed();
        assert_eq!(shared.tick(start + Duration::from_secs(1)), Tick::default());

        let tick = shared.tick(start + INTERVAL);
        let beat = tick.beat.expect("the interval elapsed");
        assert_eq!(beat.observed, 1);
        assert!(!tick.restart, "no game traffic is not a wedge");
    }

    /// A real wedge still reaches the restart verdict through the same
    /// shared path the watchdog uses, and says so in the same tick that
    /// reports the silent window.
    #[test]
    fn a_wedge_reaches_the_restart_verdict_through_a_tick() {
        let start = Instant::now();
        let shared = SharedMonitor::with_limits(start, INTERVAL, STALL_AFTER);
        for second in 0..=STALL_AFTER.as_secs() {
            shared.record_observed();
            shared.record_packet(start + Duration::from_secs(second));
        }
        let tick = shared.tick(start + STALL_AFTER);
        assert!(tick.restart, "packets arriving, nothing delivered: #214");
        assert_eq!(
            tick.beat.expect("the interval elapsed").kind(),
            HeartbeatKind::Wedged
        );
    }

    /// The end-to-end shape of the fix: the ticker produces heartbeats
    /// without a single packet ever being recorded. Before #271 this window
    /// produced nothing at all, because `poll` was only reachable from
    /// below the packet loop's `continue` ladder.
    #[test]
    fn the_watchdog_ticks_with_no_packets_at_all() {
        let interval = Duration::from_millis(20);
        let shared = SharedMonitor::with_limits(Instant::now(), interval, STALL_AFTER);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let thread_monitor = shared.clone();
        let thread_stop = Arc::clone(&stop);
        let joined = thread::spawn(move || {
            run_watchdog(
                &thread_monitor,
                &thread_stop,
                Duration::from_millis(1),
                |tick| {
                    if let Some(beat) = tick.beat {
                        let _ = tx.send((beat, tick.restart));
                    }
                },
            );
        });

        let (beat, restart) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a heartbeat must arrive with no packets whatsoever");
        stop.store(true, Ordering::Relaxed);
        joined.join().expect("the watchdog thread must exit");

        assert_eq!(beat.observed, 0);
        assert_eq!(beat.packets, 0);
        assert_eq!(beat.bytes, 0);
        assert_eq!(beat.kind(), HeartbeatKind::LinkSilent);
        assert!(
            !restart,
            "an idle link must never trigger the self-restart path"
        );
    }

    #[test]
    fn the_watchdog_stops_when_asked() {
        let shared = SharedMonitor::new(Instant::now());
        let stop = AtomicBool::new(true);
        run_watchdog(&shared, &stop, Duration::from_secs(3_600), |_| {
            panic!("a stopped watchdog must not tick")
        });
    }
}
