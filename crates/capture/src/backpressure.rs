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

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use bpsr_protocol::ProtocolEvent;
use crossbeam_channel::{Sender, TrySendError};

/// How often a sustained stall's drop count is allowed to reach the log —
/// long enough that a wedge does not spam the log at packet rate, short
/// enough that the loss is visible well inside the stall that caused it.
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A field-free cumulative view of capture's protocol-event handoff.
///
/// `depth_high_water` is sampled immediately after successful sends and
/// rejected sends. It is useful pressure context, not an atomic history of
/// the channel: the pipeline may drain between a sender's sample and a later
/// reader's observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueTelemetry {
    pub accepted: u64,
    pub dropped: u64,
    pub depth_high_water: usize,
}

#[derive(Debug, Default)]
struct QueueTelemetryState {
    generation: AtomicU64,
    accepted: AtomicU64,
    dropped: AtomicU64,
    depth_high_water: AtomicUsize,
}

/// A lock-free, field-free capture-to-pipeline diagnostic signal. Besides the
/// rate-limited drop-report generation, it holds cumulative ingress and queue
/// pressure counters so the pipeline can report rates without touching the
/// capture thread or decoded event contents.
#[derive(Clone, Debug, Default)]
pub struct QueueDropSignal(Arc<QueueTelemetryState>);

impl QueueDropSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks one completed capture-side drop-report window.
    pub fn note_report(&self) {
        self.0.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the current report generation for a consumer to compare with
    /// its last observation. A missed intermediate increment still means one
    /// aggregate report is due; no correctness depends on the exact count.
    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Relaxed)
    }

    /// Records one successfully accepted event and its queue depth sampled
    /// immediately afterwards. This is deliberately constant-time and never
    /// waits on the pipeline.
    pub fn note_accepted(&self, depth: usize) {
        self.0.accepted.fetch_add(1, Ordering::Relaxed);
        self.note_depth(depth);
    }

    /// Records one event dropped because the bounded event channel was full.
    pub fn note_dropped(&self, depth: usize) {
        self.0.dropped.fetch_add(1, Ordering::Relaxed);
        self.note_depth(depth);
    }

    fn note_depth(&self, depth: usize) {
        let mut previous = self.0.depth_high_water.load(Ordering::Relaxed);
        while depth > previous {
            match self.0.depth_high_water.compare_exchange_weak(
                previous,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => previous = observed,
            }
        }
    }

    /// Returns cumulative counters for a consumer to difference over its own
    /// reporting window. The values are approximate under concurrent capture,
    /// which is sufficient for observability and keeps the hot path lock-free.
    pub fn telemetry(&self) -> QueueTelemetry {
        QueueTelemetry {
            accepted: self.0.accepted.load(Ordering::Relaxed),
            dropped: self.0.dropped.load(Ordering::Relaxed),
            depth_high_water: self.0.depth_high_water.load(Ordering::Relaxed),
        }
    }
}

/// A privacy-safe category for one decoded event. Kept separate from the
/// event's `Debug` form: drop diagnostics must be useful in a public issue
/// without ever copying decoded fields such as player names or ids into logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Damage,
    Cast,
    Player,
    EnemyHp,
    Scene,
    ServerChanged,
    DungeonState,
    DungeonObjective,
    DungeonObjectiveRemoved,
    DungeonVar,
    EnemyGone,
    BuffApply,
    BuffRemove,
    EntityState,
    Revive,
    TeamMemberLeft,
    TeamRoster,
    LocalPlayer,
}

impl EventKind {
    const ALL: [Self; 18] = [
        Self::Damage,
        Self::Cast,
        Self::Player,
        Self::EnemyHp,
        Self::Scene,
        Self::ServerChanged,
        Self::DungeonState,
        Self::DungeonObjective,
        Self::DungeonObjectiveRemoved,
        Self::DungeonVar,
        Self::EnemyGone,
        Self::BuffApply,
        Self::BuffRemove,
        Self::EntityState,
        Self::Revive,
        Self::TeamMemberLeft,
        Self::TeamRoster,
        Self::LocalPlayer,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    /// Returns a deliberately field-free label for logs and tests.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Damage => "damage",
            Self::Cast => "cast",
            Self::Player => "player",
            Self::EnemyHp => "enemy_hp",
            Self::Scene => "scene",
            Self::ServerChanged => "server_changed",
            Self::DungeonState => "dungeon_state",
            Self::DungeonObjective => "dungeon_objective",
            Self::DungeonObjectiveRemoved => "dungeon_objective_removed",
            Self::DungeonVar => "dungeon_var",
            Self::EnemyGone => "enemy_gone",
            Self::BuffApply => "buff_apply",
            Self::BuffRemove => "buff_remove",
            Self::EntityState => "entity_state",
            Self::Revive => "revive",
            Self::TeamMemberLeft => "team_member_left",
            Self::TeamRoster => "team_roster",
            Self::LocalPlayer => "local_player",
        }
    }
}

impl From<&ProtocolEvent> for EventKind {
    fn from(event: &ProtocolEvent) -> Self {
        match event {
            ProtocolEvent::Damage(_) => Self::Damage,
            ProtocolEvent::Cast(_) => Self::Cast,
            ProtocolEvent::Player(_) => Self::Player,
            ProtocolEvent::EnemyHp(_) => Self::EnemyHp,
            ProtocolEvent::Scene { .. } => Self::Scene,
            ProtocolEvent::ServerChanged => Self::ServerChanged,
            ProtocolEvent::DungeonState { .. } => Self::DungeonState,
            ProtocolEvent::DungeonObjective { .. } => Self::DungeonObjective,
            ProtocolEvent::DungeonObjectiveRemoved { .. } => Self::DungeonObjectiveRemoved,
            ProtocolEvent::DungeonVar { .. } => Self::DungeonVar,
            ProtocolEvent::EnemyGone { .. } => Self::EnemyGone,
            ProtocolEvent::BuffApply { .. } => Self::BuffApply,
            ProtocolEvent::BuffRemove { .. } => Self::BuffRemove,
            ProtocolEvent::EntityState { .. } => Self::EntityState,
            ProtocolEvent::Revive { .. } => Self::Revive,
            ProtocolEvent::TeamMemberLeft { .. } => Self::TeamMemberLeft,
            ProtocolEvent::TeamRoster { .. } => Self::TeamRoster,
            ProtocolEvent::LocalPlayer { .. } => Self::LocalPlayer,
        }
    }
}

/// Bounded, field-free details about the drops folded into one log window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropReport {
    pub total: u64,
    /// Lowest and highest queue lengths observed immediately after a rejected
    /// send. `try_send` established that the queue was full at the send; the
    /// receiver can drain before these samples are read, so they are context,
    /// not a claim of an atomic queue history.
    pub observed_depth: (usize, usize),
    pub capacity: Option<usize>,
    kinds: [u64; EventKind::ALL.len()],
}

impl DropReport {
    /// Returns only categories that actually lost events, in stable order.
    pub fn kinds(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        EventKind::ALL.into_iter().filter_map(|kind| {
            let count = self.kinds[kind.index()];
            (count != 0).then_some((kind.label(), count))
        })
    }
}

/// What [`DropCounter::try_send`] learned from one send attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    Dropped(Option<DropReport>),
    /// The receiving end is gone; the caller should stop sending.
    Disconnected,
}

/// Tracks events dropped by a full channel and decides when that count is
/// due to be logged. One instance per capture loop invocation.
#[derive(Debug, Default)]
pub struct DropCounter {
    dropped_since_log: u64,
    last_logged_at: Option<Instant>,
    min_observed_depth: usize,
    max_observed_depth: usize,
    capacity: Option<usize>,
    dropped_by_kind: [u64; EventKind::ALL.len()],
}

impl DropCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends `event` on `tx` without ever blocking. See the module doc for
    /// why: a stalled pipeline must not stall the capture thread behind it.
    pub fn try_send(
        &mut self,
        tx: &Sender<ProtocolEvent>,
        event: ProtocolEvent,
        now: Instant,
    ) -> SendOutcome {
        match tx.try_send(event) {
            Ok(()) => SendOutcome::Sent,
            Err(TrySendError::Full(event)) => {
                let depth = tx.len();
                SendOutcome::Dropped(self.record_drop(
                    now,
                    EventKind::from(&event),
                    depth,
                    tx.capacity(),
                ))
            }
            Err(TrySendError::Disconnected(_)) => SendOutcome::Disconnected,
        }
    }

    /// Folds one drop into the running count and decides whether it is due
    /// to be logged, resetting the count for the next window when it is.
    fn record_drop(
        &mut self,
        now: Instant,
        kind: EventKind,
        observed_depth: usize,
        capacity: Option<usize>,
    ) -> Option<DropReport> {
        self.dropped_since_log += 1;
        self.dropped_by_kind[kind.index()] += 1;
        if self.dropped_since_log == 1 {
            self.min_observed_depth = observed_depth;
            self.max_observed_depth = observed_depth;
            self.capacity = capacity;
        } else {
            self.min_observed_depth = self.min_observed_depth.min(observed_depth);
            self.max_observed_depth = self.max_observed_depth.max(observed_depth);
        }
        let due = match self.last_logged_at {
            None => true,
            Some(last) => now.duration_since(last) >= LOG_INTERVAL,
        };
        if !due {
            return None;
        }
        let report = DropReport {
            total: self.dropped_since_log,
            observed_depth: (self.min_observed_depth, self.max_observed_depth),
            capacity: self.capacity,
            kinds: self.dropped_by_kind,
        };
        self.dropped_since_log = 0;
        self.dropped_by_kind = [0; EventKind::ALL.len()];
        self.last_logged_at = Some(now);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn server_changed() -> ProtocolEvent {
        ProtocolEvent::ServerChanged
    }

    #[test]
    fn a_send_with_room_succeeds_and_counts_nothing() {
        let (tx, rx) = bounded::<ProtocolEvent>(1);
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert_eq!(
            counter.try_send(&tx, server_changed(), now),
            SendOutcome::Sent
        );
        assert_eq!(rx.try_recv(), Ok(server_changed()));
    }

    #[test]
    fn the_first_drop_on_a_full_channel_is_due_immediately() {
        let (tx, _rx) = bounded::<ProtocolEvent>(1);
        tx.try_send(server_changed()).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        let SendOutcome::Dropped(Some(report)) = counter.try_send(&tx, server_changed(), now)
        else {
            panic!("a full channel must drop and report the first event");
        };
        assert_eq!(report.total, 1);
        assert_eq!(
            report.kinds().collect::<Vec<_>>(),
            vec![("server_changed", 1)]
        );
    }

    #[test]
    fn drops_inside_the_log_interval_accumulate_silently() {
        let (tx, _rx) = bounded::<ProtocolEvent>(1);
        tx.try_send(server_changed()).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert!(matches!(
            counter.try_send(&tx, server_changed(), now),
            SendOutcome::Dropped(Some(_))
        ));
        assert_eq!(
            counter.try_send(&tx, server_changed(), now + Duration::from_secs(1)),
            SendOutcome::Dropped(None)
        );
        assert_eq!(
            counter.try_send(&tx, server_changed(), now + Duration::from_millis(4_999)),
            SendOutcome::Dropped(None)
        );
    }

    #[test]
    fn a_drop_at_the_interval_boundary_logs_the_whole_window_and_resets() {
        let (tx, _rx) = bounded::<ProtocolEvent>(1);
        tx.try_send(server_changed()).unwrap();
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert!(matches!(
            counter.try_send(&tx, server_changed(), now),
            SendOutcome::Dropped(Some(_))
        ));
        assert_eq!(
            counter.try_send(&tx, server_changed(), now + Duration::from_secs(2)),
            SendOutcome::Dropped(None)
        );
        let SendOutcome::Dropped(Some(report)) =
            counter.try_send(&tx, server_changed(), now + LOG_INTERVAL)
        else {
            panic!("the interval boundary must report the accumulated window");
        };
        assert_eq!(report.total, 2);

        // The window reset, so the very next drop starts a fresh count
        // rather than being due immediately again.
        assert_eq!(
            counter.try_send(
                &tx,
                server_changed(),
                now + LOG_INTERVAL + Duration::from_millis(1)
            ),
            SendOutcome::Dropped(None)
        );
    }

    #[test]
    fn a_disconnected_receiver_is_reported_and_never_counted_as_a_drop() {
        let (tx, rx) = bounded::<ProtocolEvent>(1);
        drop(rx);
        let mut counter = DropCounter::new();

        assert_eq!(
            counter.try_send(&tx, server_changed(), Instant::now()),
            SendOutcome::Disconnected
        );
    }

    #[test]
    fn a_report_preserves_kind_counts_and_the_observed_depth_range() {
        let mut counter = DropCounter::new();
        let now = Instant::now();

        assert!(
            counter
                .record_drop(now, EventKind::Damage, 4096, Some(4096))
                .is_some()
        );
        assert!(
            counter
                .record_drop(
                    now + Duration::from_secs(1),
                    EventKind::Cast,
                    4094,
                    Some(4096),
                )
                .is_none()
        );
        let report = counter
            .record_drop(now + LOG_INTERVAL, EventKind::Damage, 4095, Some(4096))
            .expect("the rate-limit boundary must produce a report");

        assert_eq!(report.total, 2, "the first drop belongs to its own window");
        assert_eq!(report.observed_depth, (4094, 4095));
        assert_eq!(report.capacity, Some(4096));
        assert_eq!(
            report.kinds().collect::<Vec<_>>(),
            vec![("damage", 1), ("cast", 1)]
        );
    }

    #[test]
    fn queue_drop_signal_is_shared_without_carrying_event_data() {
        let signal = QueueDropSignal::new();
        let observer = signal.clone();

        assert_eq!(observer.generation(), 0);
        signal.note_report();
        assert_eq!(observer.generation(), 1);
    }

    #[test]
    fn queue_telemetry_is_shared_and_tracks_ingress_drops_and_high_water() {
        let signal = QueueDropSignal::new();
        let observer = signal.clone();

        signal.note_accepted(2);
        signal.note_dropped(4);
        signal.note_accepted(3);

        assert_eq!(
            observer.telemetry(),
            QueueTelemetry {
                accepted: 2,
                dropped: 1,
                depth_high_water: 4,
            }
        );
    }
}
