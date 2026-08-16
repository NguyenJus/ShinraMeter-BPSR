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
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, tick};

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
        proto::Class::TwinStriker => meter::Class::TwinStriker,
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
            is_dead: d.is_dead,
        }),
        proto::ProtocolEvent::Player(p) => meter::ProtocolEvent::Player(meter::PlayerInfo {
            uid: p.uid,
            name: p.name,
            class: p.class.map(map_class),
            ability_score: p.ability_score,
            season_strength: p.season_strength,
        }),
        proto::ProtocolEvent::EnemyHp(e) => meter::ProtocolEvent::EnemyHp(meter::EnemyHp {
            uid: e.uid,
            curr_hp: e.curr_hp,
            max_hp: e.max_hp,
            monster_id: e.monster_id,
            timestamp_ms: e.timestamp_ms,
        }),
        proto::ProtocolEvent::Scene { level_map_id } => {
            meter::ProtocolEvent::Scene { level_map_id }
        }
        proto::ProtocolEvent::ServerChanged => meter::ProtocolEvent::ServerChanged {
            timestamp_ms: now_ms(),
        },
    }
}

/// A name-cache snapshot as handed to the background writer thread — the
/// same shape `meter::names_cache::save` takes.
type NamesCacheSnapshot = Vec<(i64, Option<String>, Option<meter::Class>)>;

/// Persists the name cache off the pipeline thread, so a slow disk can never
/// stall the `select!` loop that drains the bounded capture-event channel.
/// The channel has capacity 1 and coalesces: a snapshot still waiting to be
/// written is dropped in favour of a newer one rather than queued, since only
/// the latest name-cache state is ever worth persisting.
struct CacheWriter {
    tx: Sender<NamesCacheSnapshot>,
    /// A second receiver handle used only to drain a stale, not-yet-written
    /// snapshot out of the channel before enqueuing a newer one (mirrors
    /// `publish`'s drop-oldest pattern for UI snapshots below).
    stale: Receiver<NamesCacheSnapshot>,
    handle: Option<JoinHandle<()>>,
}

impl CacheWriter {
    fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = bounded::<NamesCacheSnapshot>(1);
        let stale = rx.clone();

        let handle = std::thread::Builder::new()
            .name("names-cache-writer".to_string())
            .spawn(move || {
                // Keeps draining until every `Sender` (the pipeline's `tx`
                // plus `stale`, once `CacheWriter` is dropped) is gone *and*
                // the channel is empty — so a snapshot enqueued right before
                // shutdown is still written before this thread exits.
                while let Ok(snapshot) = rx.recv() {
                    meter::names_cache::save(&path, &snapshot);
                }
            })
            .expect("failed to spawn the names-cache-writer thread");

        Self {
            tx,
            stale,
            handle: Some(handle),
        }
    }

    /// Enqueues `snapshot` to be written, dropping a still-pending stale
    /// snapshot in its favour rather than blocking on the writer thread.
    fn save(&self, snapshot: NamesCacheSnapshot) {
        match self.tx.try_send(snapshot) {
            Ok(()) => {}
            Err(TrySendError::Full(snapshot)) => {
                let _ = self.stale.try_recv();
                let _ = self.tx.try_send(snapshot);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Closes the channel and blocks until the writer thread has drained
    /// (and written) any pending snapshot. Must be called before process
    /// exit so the final save is never lost.
    fn shutdown(self) {
        drop(self.tx);
        drop(self.stale);
        if let Some(handle) = self.handle {
            let _ = handle.join();
        }
    }
}

/// Owns the encounter state and applies mapped protocol events to it.
pub struct Pipeline {
    meter: meter::Meter,
    /// Background writer for the cross-session name cache (issue #12).
    /// `None` in tests / `Pipeline::new()` — no writer means no disk IO at
    /// all.
    cache_writer: Option<CacheWriter>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            meter: meter::Meter::new(),
            cache_writer: None,
        }
    }

    /// Loads the uid -> (name, class) cache from `path` (if it exists) to
    /// seed the meter, and spawns a background writer against `path` so
    /// future resets/shutdown persist back to it off the pipeline thread. A
    /// missing or corrupt file is not an error — the meter simply starts
    /// with an empty cache (see `meter::names_cache::load`).
    pub fn with_names_cache_path(path: PathBuf) -> Self {
        let cached = meter::names_cache::load(&path);
        Self {
            meter: meter::Meter::with_names_cache(cached),
            cache_writer: Some(CacheWriter::spawn(path)),
        }
    }

    /// Hands the current name cache to the background writer, if one is
    /// configured. Cheap on the calling thread: it only clones and enqueues
    /// (see `CacheWriter::save`), never blocks on disk IO. Never panics —
    /// `meter::names_cache::save` (run on the writer thread) logs and
    /// swallows IO errors.
    pub fn save_names_cache(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.save(self.meter.names_for_save());
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

    /// Advances the meter's wall-clock-driven fight state (issue #78) —
    /// specifically, the idle timeout that ends a fight and freezes its
    /// stats on screen. Called once per publish tick, immediately before
    /// `snapshot`, because no packet arrives to mark the moment a fight goes
    /// quiet.
    /// Returns the resulting state, which the UI can use later to label a
    /// held meter (`bpsr_meter::Meter::fight_state` answers the same question
    /// without advancing anything).
    pub fn tick(&mut self, now_ms: u64) -> meter::FightState {
        self.meter.tick(now_ms)
    }

    /// Stops the background cache-writer thread, blocking until it has
    /// written any pending snapshot to disk. Must be called before process
    /// exit (see `run`'s shutdown path) so the final save is never lost; a
    /// no-op if no writer is configured (`Pipeline::new()`, or already shut
    /// down). Tests that need a synchronous view of the on-disk cache after
    /// a `step`/`reset`-triggered save call this instead of sleeping.
    pub fn shutdown_names_cache(&mut self) {
        if let Some(writer) = self.cache_writer.take() {
            writer.shutdown();
        }
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
            recv(ticker) -> _ => publish(&mut pipeline, &tx_snapshot, &stale),
        }
    }

    // Shutdown save: catches identity data learned since the last
    // reset/encounter-end save (e.g. a session with no resets at all). Then
    // stop the writer thread, blocking until it has drained this (and any
    // still-pending) snapshot to disk, so the final save is never lost.
    pipeline.save_names_cache();
    pipeline.shutdown_names_cache();
}

/// Publishes the latest snapshot, dropping the previous one if the UI has not
/// consumed it yet.
fn publish(
    pipeline: &mut Pipeline,
    tx_snapshot: &Sender<meter::Snapshot>,
    stale: &Receiver<meter::Snapshot>,
) {
    // One `now` for the whole tick: the fight-state advance and the snapshot
    // it feeds must agree on what time it is.
    let now = now_ms();
    pipeline.tick(now);
    let snap = pipeline.snapshot(now);
    if tx_snapshot.try_send(snap).is_err() {
        let _ = stale.try_recv();
        let _ = tx_snapshot.try_send(pipeline.snapshot(now));
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
            is_dead: false,
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
            (proto::Class::TwinStriker, meter::Class::TwinStriker),
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
        assert_eq!(m.is_dead, d.is_dead);
    }

    #[test]
    fn maps_is_dead_true() {
        let d = proto::DamageEvent {
            is_dead: true,
            ..damage(11, 1234, 9_000)
        };
        let mapped = map_event(proto::ProtocolEvent::Damage(d));
        let meter::ProtocolEvent::Damage(m) = mapped else {
            panic!("expected a damage event");
        };
        assert!(m.is_dead);
    }

    #[test]
    fn maps_player_info() {
        let mapped = map_event(proto::ProtocolEvent::Player(proto::PlayerInfo {
            uid: 42,
            name: Some("Foo".to_string()),
            class: Some(proto::Class::Marksman),
            ability_score: Some(9_999),
            season_level: Some(42),
            season_strength: Some(3_333),
        }));
        assert_eq!(
            mapped,
            meter::ProtocolEvent::Player(meter::PlayerInfo {
                uid: 42,
                name: Some("Foo".to_string()),
                class: Some(meter::Class::Marksman),
                ability_score: Some(9_999),
                season_strength: Some(3_333),
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
    fn maps_scene_event() {
        let mapped = map_event(proto::ProtocolEvent::Scene { level_map_id: 4242 });
        assert_eq!(mapped, meter::ProtocolEvent::Scene { level_map_id: 4242 });
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

    /// Issue #78: the pipeline's tick is what turns "no packets for a while"
    /// into a frozen meter, so the wiring is worth pinning here and not only
    /// in `bpsr-meter`.
    #[test]
    fn a_quiet_meter_ticks_into_the_ended_state_and_holds_its_snapshot() {
        let idle = meter::FightConfig::default().idle_timeout_ms;
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)));

        assert_eq!(p.tick(1_000 + idle - 1), meter::FightState::Active);
        assert_eq!(p.tick(1_000 + idle), meter::FightState::Ended);
        assert_eq!(p.tick(600_000), meter::FightState::Ended);

        // Held: the elapsed timer and the totals stop moving.
        let held = p.snapshot(600_000);
        assert_eq!(held.total_damage, 700);
        assert_eq!(held.duration_ms, 1);
    }

    #[test]
    fn the_next_fights_first_hit_clears_a_held_snapshot() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)));
        p.tick(600_000);

        let reason = p.step(proto::ProtocolEvent::Damage(damage(1, 300, 600_000)));
        assert_eq!(reason, Some(meter::ResetReason::NewFight));
        assert_eq!(p.snapshot(601_000).total_damage, 300);
        assert_eq!(p.tick(601_000), meter::FightState::Active);
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
        use bpsr_test_support::scratch_path;

        /// Looks up `uid` in a `names_cache::load` result (order-preserving
        /// `Vec`, not a map) and returns its cached name.
        fn cached_name(loaded: &meter::names_cache::LoadedNames, uid: i64) -> Option<String> {
            loaded
                .iter()
                .find(|(u, _)| *u == uid)
                .and_then(|(_, (name, _))| name.clone())
        }

        #[test]
        fn with_names_cache_path_seeds_the_meter_from_an_existing_file() {
            let path = scratch_path("seed");
            meter::names_cache::save(
                &path,
                &[(5, Some("Cached".to_string()), Some(meter::Class::Marksman))],
            );

            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(proto::ProtocolEvent::Damage(damage(5, 100, 1_000)));
            let snap = p.snapshot(2_000);
            assert_eq!(snap.rows[0].name, "Cached");

            p.shutdown_names_cache();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn manual_reset_persists_the_names_cache_to_disk() {
            let path = scratch_path("manual-reset");
            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: None,
            }));

            assert!(!path.exists());
            p.reset(1_000);
            // The save is handed off to the background writer thread; flush
            // and join it before asserting the on-disk state.
            p.shutdown_names_cache();
            assert!(path.exists());

            let loaded = meter::names_cache::load(&path);
            assert_eq!(cached_name(&loaded, 1), Some("Foo".to_string()));

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn step_triggered_reset_persists_the_names_cache_to_disk() {
            // Covers the branch that actually fires during real play:
            // `step()` driving a reset (here, a server change) persists the
            // cache via the background writer, not just the manual-reset
            // path exercised above.
            let path = scratch_path("step-reset");
            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: None,
            }));

            assert!(!path.exists());
            let reason = p.step(proto::ProtocolEvent::ServerChanged);
            assert_eq!(reason, Some(meter::ResetReason::ServerChange));

            p.shutdown_names_cache();
            assert!(path.exists());

            let loaded = meter::names_cache::load(&path);
            assert_eq!(cached_name(&loaded, 1), Some("Foo".to_string()));

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
            ability_score: None,
            season_level: None,
            season_strength: None,
        }));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].name, "Late");
        assert_eq!(snap.rows[0].class, Some(meter::Class::FrostMage));
    }

    #[test]
    fn ability_score_flows_from_protocol_player_info_to_the_snapshot_row() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(9, 100, 1_000)));
        p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: Some(77_000),
            season_level: None,
            season_strength: None,
        }));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].ability_score, Some(77_000));
    }

    #[test]
    fn season_strength_flows_from_protocol_player_info_to_the_snapshot_row() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(10, 100, 1_000)));
        p.step(proto::ProtocolEvent::Player(proto::PlayerInfo {
            uid: 10,
            name: None,
            class: None,
            ability_score: None,
            season_level: None,
            season_strength: Some(3_333),
        }));
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].season_strength, Some(3_333));
    }
}
