//! Capture -> meter -> UI wiring (plan §T4.2).
//!
//! `bpsr-meter` deliberately does not depend on `bpsr-protocol`; it mirrors the
//! protocol crate's event contract in `bpsr_meter::event`. This module owns the
//! translation between the two, plus the background thread that applies events
//! to the `Meter` and publishes `Snapshot`s to the overlay at ~10 Hz.
//!
//! Timestamps: `bpsr-meter` is timestamp-pure (every entry point takes an
//! explicit `now_ms`), and `bpsr-protocol` events already carry the capture
//! thread's timestamp. [`now_ms`] is this crate's only `SystemTime` call site.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bpsr_meter as meter;
use bpsr_protocol as proto;
use crossbeam_channel::{Receiver, Sender, bounded, select, tick};

use crate::ui::UiCommand;

/// Snapshot publication rate (~10 Hz), matching the overlay's repaint cadence.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Wall-clock milliseconds since the Unix epoch — the single `SystemTime` call
/// site in the app.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Maps a protocol entity kind onto the meter's mirror type.
pub fn map_kind(kind: proto::EntityKind) -> meter::EntityKind {
    match kind {
        proto::EntityKind::Player => meter::EntityKind::Player,
        proto::EntityKind::Monster => meter::EntityKind::Monster,
        proto::EntityKind::Unknown => meter::EntityKind::Unknown,
    }
}

/// Maps a protocol class onto the meter's mirror type.
pub fn map_class(class: proto::Class) -> meter::Class {
    match class {
        proto::Class::Stormblade => meter::Class::Stormblade,
        proto::Class::FrostMage => meter::Class::FrostMage,
        proto::Class::WindKnight => meter::Class::WindKnight,
        proto::Class::VerdantOracle => meter::Class::VerdantOracle,
        proto::Class::HeavyGuardian => meter::Class::HeavyGuardian,
        proto::Class::Marksman => meter::Class::Marksman,
        proto::Class::ShieldKnight => meter::Class::ShieldKnight,
        proto::Class::BeatPerformer => meter::Class::BeatPerformer,
        proto::Class::Unknown => meter::Class::Unknown,
    }
}

/// Translates a `bpsr_protocol::ProtocolEvent` into the meter's mirror event.
pub fn map_event(ev: proto::ProtocolEvent) -> meter::ProtocolEvent {
    match ev {
        proto::ProtocolEvent::Damage(d) => meter::ProtocolEvent::Damage(meter::DamageEvent {
            attacker_uid: d.attacker_uid,
            attacker_kind: map_kind(d.attacker_kind),
            skill_id: d.skill_id,
            value: d.value,
            crit: d.crit,
            lucky: d.lucky,
            hp_lessen: d.hp_lessen,
            is_miss: d.is_miss,
            is_heal: d.is_heal,
            target_uid: d.target_uid,
            target_kind: map_kind(d.target_kind),
            timestamp_ms: d.timestamp_ms,
        }),
        proto::ProtocolEvent::Player(p) => meter::ProtocolEvent::Player(meter::PlayerInfo {
            uid: p.uid,
            name: p.name,
            class: p.class.map(map_class),
        }),
        proto::ProtocolEvent::EnemyHp(e) => meter::ProtocolEvent::EnemyHp(meter::EnemyHp {
            uid: e.uid,
            curr_hp: e.curr_hp,
            max_hp: e.max_hp,
            monster_id: e.monster_id,
            timestamp_ms: e.timestamp_ms,
        }),
        proto::ProtocolEvent::ServerChanged => meter::ProtocolEvent::ServerChanged {
            timestamp_ms: now_ms(),
        },
    }
}

/// Owns the encounter state and applies mapped protocol events to it.
pub struct Pipeline {
    meter: meter::Meter,
    /// Where the cross-session name cache (issue #12) is persisted. `None`
    /// in tests / `Pipeline::new()` — no path means no disk IO at all.
    names_cache_path: Option<PathBuf>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            meter: meter::Meter::new(),
            names_cache_path: None,
        }
    }

    /// Loads the uid -> (name, class) cache from `path` (if it exists) to
    /// seed the meter, and remembers `path` so future resets/shutdown
    /// persist back to it. A missing or corrupt file is not an error — the
    /// meter simply starts with an empty cache (see `meter::names_cache::load`).
    pub fn with_names_cache_path(path: PathBuf) -> Self {
        let cached = meter::names_cache::load(&path);
        Self {
            meter: meter::Meter::with_names_cache(cached),
            names_cache_path: Some(path),
        }
    }

    /// Persists the current name cache, if a path was configured. Cheap
    /// relative to per-packet work since it's only called on
    /// reset/encounter-end and on shutdown (never per-packet). Never panics
    /// — `meter::names_cache::save` logs and swallows IO errors.
    pub fn save_names_cache(&self) {
        if let Some(path) = &self.names_cache_path {
            meter::names_cache::save(path, &self.meter.names_for_save());
        }
    }

    /// Applies one protocol event. Returns `Some(reason)` when the event
    /// triggered a reset (boss-HP rollback or server change).
    pub fn step(&mut self, ev: proto::ProtocolEvent) -> Option<meter::ResetReason> {
        let reason = self.meter.apply(&map_event(ev));
        if let Some(reason) = reason {
            log::debug!("meter reset: {reason:?}");
            self.save_names_cache();
        }
        reason
    }

    /// Manual reset, triggered by the overlay's Reset button.
    pub fn reset(&mut self, now_ms: u64) {
        self.meter.reset(meter::ResetReason::Manual, now_ms);
        self.save_names_cache();
    }

    pub fn snapshot(&self, now_ms: u64) -> meter::Snapshot {
        self.meter.snapshot(now_ms)
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawns the pipeline thread: consumes capture events, applies them to the
/// `Meter`, honours `UiCommand`s, and publishes a `Snapshot` every
/// [`TICK_INTERVAL`].
///
/// The snapshot channel has capacity 1 and is drop-oldest, so a stalled UI
/// never back-pressures into capture. The thread exits on `UiCommand::Quit` or
/// when the command channel disconnects (the overlay window closed).
pub fn spawn(
    events: Receiver<proto::ProtocolEvent>,
    commands: Receiver<UiCommand>,
    names_cache_path: PathBuf,
) -> (Receiver<meter::Snapshot>, JoinHandle<()>) {
    let (tx_snapshot, rx_snapshot) = bounded::<meter::Snapshot>(1);
    let stale = rx_snapshot.clone();

    let handle = std::thread::Builder::new()
        .name("pipeline".to_string())
        .spawn(move || run(events, commands, tx_snapshot, stale, names_cache_path))
        .expect("failed to spawn the pipeline thread");

    (rx_snapshot, handle)
}

fn run(
    events: Receiver<proto::ProtocolEvent>,
    commands: Receiver<UiCommand>,
    tx_snapshot: Sender<meter::Snapshot>,
    stale: Receiver<meter::Snapshot>,
    names_cache_path: PathBuf,
) {
    let mut pipeline = Pipeline::with_names_cache_path(names_cache_path);
    // Replaced by `never()` once capture disconnects, so a dead channel does
    // not spin the select loop.
    let mut events = events;
    let ticker = tick(TICK_INTERVAL);

    loop {
        select! {
            recv(events) -> msg => match msg {
                Ok(ev) => {
                    pipeline.step(ev);
                }
                Err(_) => {
                    log::info!("capture channel closed; pipeline is idle");
                    events = crossbeam_channel::never();
                }
            },
            recv(commands) -> msg => match msg {
                Ok(UiCommand::Reset) => pipeline.reset(now_ms()),
                Ok(UiCommand::Quit) => break,
                Err(_) => break,
            },
            recv(ticker) -> _ => publish(&pipeline, &tx_snapshot, &stale),
        }
    }

    // Shutdown save: catches identity data learned since the last
    // reset/encounter-end save (e.g. a session with no resets at all).
    pipeline.save_names_cache();
}

/// Publishes the latest snapshot, dropping the previous one if the UI has not
/// consumed it yet.
fn publish(
    pipeline: &Pipeline,
    tx_snapshot: &Sender<meter::Snapshot>,
    stale: &Receiver<meter::Snapshot>,
) {
    let snap = pipeline.snapshot(now_ms());
    if tx_snapshot.try_send(snap).is_err() {
        let _ = stale.try_recv();
        let _ = tx_snapshot.try_send(pipeline.snapshot(now_ms()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn damage(attacker_uid: i64, value: i64, ts: u64) -> proto::DamageEvent {
        proto::DamageEvent {
            attacker_uid,
            attacker_kind: proto::EntityKind::Player,
            skill_id: 7,
            value,
            crit: true,
            lucky: false,
            hp_lessen: value - 1,
            is_miss: false,
            is_heal: false,
            target_uid: 500,
            target_kind: proto::EntityKind::Monster,
            timestamp_ms: ts,
        }
    }

    #[test]
    fn maps_every_entity_kind() {
        assert_eq!(
            map_kind(proto::EntityKind::Player),
            meter::EntityKind::Player
        );
        assert_eq!(
            map_kind(proto::EntityKind::Monster),
            meter::EntityKind::Monster
        );
        assert_eq!(
            map_kind(proto::EntityKind::Unknown),
            meter::EntityKind::Unknown
        );
    }

    #[test]
    fn maps_every_class() {
        let pairs = [
            (proto::Class::Stormblade, meter::Class::Stormblade),
            (proto::Class::FrostMage, meter::Class::FrostMage),
            (proto::Class::WindKnight, meter::Class::WindKnight),
            (proto::Class::VerdantOracle, meter::Class::VerdantOracle),
            (proto::Class::HeavyGuardian, meter::Class::HeavyGuardian),
            (proto::Class::Marksman, meter::Class::Marksman),
            (proto::Class::ShieldKnight, meter::Class::ShieldKnight),
            (proto::Class::BeatPerformer, meter::Class::BeatPerformer),
            (proto::Class::Unknown, meter::Class::Unknown),
        ];
        for (from, to) in pairs {
            assert_eq!(map_class(from), to, "class {from:?} mapped wrong");
        }
    }

    #[test]
    fn maps_damage_event_field_for_field() {
        let d = damage(11, 1234, 9_000);
        let mapped = map_event(proto::ProtocolEvent::Damage(d.clone()));
        let meter::ProtocolEvent::Damage(m) = mapped else {
            panic!("expected a damage event");
        };
        assert_eq!(m.attacker_uid, d.attacker_uid);
        assert_eq!(m.attacker_kind, meter::EntityKind::Player);
        assert_eq!(m.skill_id, d.skill_id);
        assert_eq!(m.value, d.value);
        assert_eq!(m.crit, d.crit);
        assert_eq!(m.lucky, d.lucky);
        assert_eq!(m.hp_lessen, d.hp_lessen);
        assert_eq!(m.is_miss, d.is_miss);
        assert_eq!(m.is_heal, d.is_heal);
        assert_eq!(m.target_uid, d.target_uid);
        assert_eq!(m.target_kind, meter::EntityKind::Monster);
        assert_eq!(m.timestamp_ms, d.timestamp_ms);
    }

    #[test]
    fn maps_player_info() {
        let mapped = map_event(proto::ProtocolEvent::Player(proto::PlayerInfo {
            uid: 42,
            name: Some("Foo".to_string()),
            class: Some(proto::Class::Marksman),
        }));
        assert_eq!(
            mapped,
            meter::ProtocolEvent::Player(meter::PlayerInfo {
                uid: 42,
                name: Some("Foo".to_string()),
                class: Some(meter::Class::Marksman),
            })
        );
    }

    #[test]
    fn maps_enemy_hp() {
        let mapped = map_event(proto::ProtocolEvent::EnemyHp(proto::EnemyHp {
            uid: 10,
            curr_hp: Some(55),
            max_hp: Some(100),
            monster_id: Some(3),
            timestamp_ms: 1_234,
        }));
        assert_eq!(
            mapped,
            meter::ProtocolEvent::EnemyHp(meter::EnemyHp {
                uid: 10,
                curr_hp: Some(55),
                max_hp: Some(100),
                monster_id: Some(3),
                timestamp_ms: 1_234,
            })
        );
    }

    #[test]
    fn maps_server_changed_stamps_the_current_wall_clock_time() {
        let before = now_ms();
        let mapped = map_event(proto::ProtocolEvent::ServerChanged);
        let after = now_ms();
        let meter::ProtocolEvent::ServerChanged { timestamp_ms } = mapped else {
            panic!("expected a server-changed event");
        };
        assert!(
            (before..=after).contains(&timestamp_ms),
            "expected {timestamp_ms} in [{before}, {after}]"
        );
    }

    #[test]
    fn damage_accumulates_into_the_snapshot() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)));
        p.step(proto::ProtocolEvent::Damage(damage(2, 300, 1_000)));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.total_damage, 1_000);
        assert_eq!(snap.rows.len(), 2);
        assert_eq!(snap.rows[0].uid, 1);
    }

    #[test]
    fn manual_reset_clears_the_snapshot() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)));
        p.reset(2_000);
        let snap = p.snapshot(3_000);
        assert_eq!(snap.total_damage, 0);
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn server_changed_resets_the_meter() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)));
        let reason = p.step(proto::ProtocolEvent::ServerChanged);
        assert_eq!(reason, Some(meter::ResetReason::ServerChange));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.total_damage, 0);
        assert!(snap.rows.is_empty());
    }

    mod names_cache_wiring {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        fn scratch_path() -> PathBuf {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!("bpsr-pipeline-names-cache-test-{n}.json"))
        }

        #[test]
        fn with_names_cache_path_seeds_the_meter_from_an_existing_file() {
            let path = scratch_path();
            meter::names_cache::save(
                &path,
                &[(5, Some("Cached".to_string()), Some(meter::Class::Marksman))],
            );

            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(proto::ProtocolEvent::Damage(damage(5, 100, 1_000)));
            let snap = p.snapshot(2_000);
            assert_eq!(snap.rows[0].name, "Cached");

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn manual_reset_persists_the_names_cache_to_disk() {
            let path = scratch_path();
            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
            }));

            assert!(!path.exists());
            p.reset(1_000);
            assert!(path.exists());

            let loaded = meter::names_cache::load(&path);
            assert_eq!(loaded[&1].0, Some("Foo".to_string()));

            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn player_info_names_a_row_that_already_has_damage() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(5, 100, 1_000)));
        p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
            uid: 5,
            name: Some("Late".to_string()),
            class: Some(proto::Class::FrostMage),
        }));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].name, "Late");
        assert_eq!(snap.rows[0].class, Some(meter::Class::FrostMage));
    }
}
