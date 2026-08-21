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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bpsr_meter as meter;
use bpsr_protocol as proto;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, tick};

use crate::imagines;
use crate::scene_bosses_cache;
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

// `map_kind`/`map_class` are shared, byte-identical with the offline
// sanitizer's copy — see `bpsr_protocol::map`'s doc comment (issue #146's
// finding 2). Re-exported (rather than called via `proto::map::` at each
// use site below) so this module's own call sites and unit tests keep
// referring to them as plain `map_kind`/`map_class`.
pub use bpsr_protocol::map::{map_class, map_kind};

// IMAGINE-TAKEDOWN: classifies raw skill ids into up to two equipped-Imagine
// slots. See `crates/app/src/imagines.rs` and the plan's D2/D4 and the spec's
// "Many-to-one is expected" section for why dedup is keyed on the whole
// `Imagine` value (equivalently, its `icon`) rather than `name` or raw id.
/// Walks `skill_ids` (each a `(skill_id, remodel_level)` pair — `remodel_level`
/// is the tier field issues #169/#170 thread through, BPSR-ZDPS's `Tier`) in
/// wire order, resolves each id through [`imagines::imagine_of_skill_id`],
/// skips unknown ids (they must never occupy a slot), and dedups by the
/// resolved `Imagine` value so that variant ids sharing one canonical
/// Imagine collapse to a single slot even when their own `name` differs
/// from the canonical one (e.g. `102651`/`102655` vs. `3905`). Returns two
/// parallel arrays — the *representative skill id* and that same id's own
/// tier, both from the first occurrence seen for each distinct Imagine —
/// for up to two distinct Imagines, in wire order. A dedup'd-away variant's
/// tier is never consulted: only the representative occurrence's tier ever
/// reaches a slot.
fn imagine_slots(skill_ids: &[(i32, i32)]) -> ([Option<i32>; 2], [Option<i32>; 2]) {
    let mut ids: [Option<i32>; 2] = [None, None];
    let mut tiers: [Option<i32>; 2] = [None, None];
    let mut seen: Vec<imagines::Imagine> = Vec::with_capacity(2);

    for &(id, tier) in skill_ids {
        let Some(imagine) = imagines::imagine_of_skill_id(id) else {
            continue;
        };
        if seen.contains(&imagine) {
            continue;
        }
        let slot_index = seen.len();
        if slot_index >= ids.len() {
            break;
        }
        seen.push(imagine);
        ids[slot_index] = Some(id);
        tiers[slot_index] = Some(tier);
    }

    (ids, tiers)
}

/// Translates a `bpsr_protocol::ProtocolEvent` into the meter's mirror
/// event. Delegates the field-for-field mapping to `bpsr_protocol::map`
/// (issue #146's finding 2 — shared with the offline sanitizer binary),
/// resolving only the app-specific `imagines`/`imagine_tiers` slot pairs
/// here first, since the Imagine catalog (`crate::imagines`) is out of
/// scope for that crate.
pub fn map_event(ev: proto::ProtocolEvent, now_ms: u64) -> meter::ProtocolEvent {
    // IMAGINE-TAKEDOWN: empty `skill_ids` means the attr was absent from
    // this packet, so stay `None` rather than `Some([None, None])` — the
    // meter's merge rule (T4) must not clobber a previously cached pair
    // with an absent packet. Same rule applies to `imagine_tiers` (issues
    // #169/#170): it is `Some` exactly when `imagines` is `Some`, never
    // independently.
    let (imagines, imagine_tiers) = match &ev {
        proto::ProtocolEvent::Player(p) if !p.skill_ids.is_empty() => {
            let (ids, tiers) = imagine_slots(&p.skill_ids);
            (Some(ids), Some(tiers))
        }
        _ => (None, None),
    };
    bpsr_protocol::map::map_event(ev, now_ms, imagines, imagine_tiers)
}

/// What one cross-session cache knows about its own file: the snapshot its
/// background writer carries, and how to persist — or delete — it. One
/// zero-sized marker type per cache, so [`CacheWriter`] can be a single
/// implementation shared by the name cache (issue #12) and the scene ->
/// final-boss cache (issue #131) rather than two near-verbatim copies.
trait CachePersist: Send + 'static {
    /// The snapshot handed across the writer's channel.
    type Snapshot: Send + 'static;

    /// Names the writer thread, so a log line or a stack trace still says
    /// which cache it belongs to.
    const THREAD_NAME: &'static str;

    /// Writes `snapshot` to `path`. Runs on the writer thread, so — like
    /// both cache modules already do — it must log and swallow IO errors
    /// rather than panic.
    fn save(path: &Path, snapshot: &Self::Snapshot);

    /// Deletes the file at `path`. Defaults to doing nothing because only
    /// the scene -> final-boss cache has a user-facing "forget" (issue
    /// #131); the name cache simply never sends [`CacheWriterMsg::Forget`].
    fn forget(_path: &Path) {}
}

/// The cross-session uid -> (name, class) cache (issue #12).
struct NamesCache;

impl CachePersist for NamesCache {
    /// The same shape `meter::names_cache::save` takes.
    type Snapshot = Vec<(i64, Option<String>, Option<meter::Class>)>;
    const THREAD_NAME: &'static str = "names-cache-writer";

    fn save(path: &Path, snapshot: &Self::Snapshot) {
        meter::names_cache::save(path, snapshot);
    }
}

/// The cross-session scene -> final-boss cache (issue #131).
struct SceneBosses;

impl CachePersist for SceneBosses {
    /// The same shape `scene_bosses_cache::save` takes.
    type Snapshot = HashMap<u32, u32>;
    const THREAD_NAME: &'static str = "scene-bosses-writer";

    fn save(path: &Path, snapshot: &Self::Snapshot) {
        scene_bosses_cache::save(path, snapshot);
    }

    fn forget(path: &Path) {
        scene_bosses_cache::forget(path);
    }
}

/// One command for a [`CacheWriter`]'s thread. Both variants state the
/// file's next contents outright — "hold exactly this" / "hold nothing" —
/// rather than amending them, which is what makes [`CacheWriter::send`]'s
/// coalescing safe.
enum CacheWriterMsg<T> {
    Save(T),
    Forget,
}

/// Persists one cache off the pipeline thread, so a slow disk can never
/// stall the `select!` loop that drains the bounded capture-event channel.
/// The channel has capacity 1 and coalesces: a command still waiting to be
/// carried out is dropped in favour of a newer one rather than queued,
/// since only the latest state of the file is ever worth reaching.
///
/// Deletes ride this same channel instead of going out of band (issue
/// #131): a `forget` that removed the file directly would race a save the
/// writer thread had not drained yet, and that queued pre-forget snapshot
/// would then recreate the file behind the user's back.
struct CacheWriter<P: CachePersist> {
    tx: Sender<CacheWriterMsg<P::Snapshot>>,
    /// A second receiver handle used only to drain a stale, not-yet-executed
    /// command out of the channel before enqueuing a newer one (mirrors
    /// `publish`'s drop-oldest pattern for UI snapshots below).
    stale: Receiver<CacheWriterMsg<P::Snapshot>>,
    handle: Option<JoinHandle<()>>,
}

impl<P: CachePersist> CacheWriter<P> {
    fn spawn(path: PathBuf) -> Self {
        Self::spawn_inner(path, None)
    }

    /// `gate`, when set, parks the writer thread before it touches the
    /// channel — a test seam (see `Pipeline::with_gated_scene_bosses_path`)
    /// for pinning down interleavings that would otherwise depend on how
    /// fast the disk is. It is always `None` outside tests.
    fn spawn_inner(path: PathBuf, gate: Option<Receiver<()>>) -> Self {
        let (tx, rx) = bounded::<CacheWriterMsg<P::Snapshot>>(1);
        let stale = rx.clone();

        let handle = std::thread::Builder::new()
            .name(P::THREAD_NAME.to_string())
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                // Keeps draining until every `Sender` (the pipeline's `tx`
                // plus `stale`, once `CacheWriter` is dropped) is gone *and*
                // the channel is empty — so a command enqueued right before
                // shutdown is still carried out before this thread exits.
                while let Ok(msg) = rx.recv() {
                    match msg {
                        CacheWriterMsg::Save(snapshot) => P::save(&path, &snapshot),
                        CacheWriterMsg::Forget => P::forget(&path),
                    }
                }
            })
            .unwrap_or_else(|err| panic!("failed to spawn the {} thread: {err}", P::THREAD_NAME));

        Self {
            tx,
            stale,
            handle: Some(handle),
        }
    }

    /// Enqueues `snapshot` to be written.
    fn save(&self, snapshot: P::Snapshot) {
        self.send(CacheWriterMsg::Save(snapshot));
    }

    /// Enqueues a delete of the cache file, ordered behind every save this
    /// writer has already been handed (issue #131's "Forget learned
    /// bosses").
    fn forget(&self) {
        self.send(CacheWriterMsg::Forget);
    }

    /// Hands `msg` to the writer thread without ever blocking on it: when
    /// the capacity-1 channel is full, the command still sitting in it is
    /// drained and this newer one takes its place.
    ///
    /// Discarding that pending command can never change what ends up on
    /// disk, because each command names the file's next contents outright:
    ///
    /// * a `Save` superseded by a newer `Save` is stale by definition —
    ///   only the latest cache state is worth persisting;
    /// * a `Save` superseded by a `Forget` *must* go: that snapshot is
    ///   exactly the data the user asked to forget, and writing it after
    ///   the delete is the race routing deletes through here closes;
    /// * a `Forget` superseded by a newer `Save` lands on that snapshot
    ///   either way, since `save` rewrites the whole file rather than
    ///   merging into it — and that snapshot is necessarily a post-forget
    ///   one, because saves and forgets are both issued by the pipeline
    ///   thread and `forget_scene_bosses` clears the in-process map before
    ///   it enqueues.
    fn send(&self, msg: CacheWriterMsg<P::Snapshot>) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(msg)) => {
                let _ = self.stale.try_recv();
                let _ = self.tx.try_send(msg);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Closes the channel and blocks until the writer thread has carried
    /// out any pending command. Must be called before process exit so the
    /// final save is never lost.
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
    cache_writer: Option<CacheWriter<NamesCache>>,
    /// Background writer for the cross-session scene -> final-boss cache
    /// (issue #131). Same "`None` means no disk IO" contract as
    /// `cache_writer`, and it owns the deletes behind "Forget learned
    /// bosses" as well as the saves — see `CacheWriter`.
    scene_bosses_writer: Option<CacheWriter<SceneBosses>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            meter: meter::Meter::new(),
            cache_writer: None,
            scene_bosses_writer: None,
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
            scene_bosses_writer: None,
        }
    }

    /// Loads the scene -> final-boss cache from `path` (if it exists) to
    /// seed the meter, and spawns a background writer against `path` so
    /// future resets/shutdown persist back to it off the pipeline thread
    /// (issue #131). Mirrors `with_names_cache_path` exactly, but takes
    /// `self` by value so it can be chained after that constructor — `run`
    /// needs both caches seeded on the one `Pipeline` it constructs, and
    /// `Meter::set_scene_bosses` (unlike `with_names_cache`, which builds a
    /// whole fresh `Meter`) is a plain in-place setter for exactly this
    /// reason.
    pub fn with_scene_bosses_path(self, path: PathBuf) -> Self {
        self.with_scene_bosses_writer(path, None)
    }

    /// Test seam: `with_scene_bosses_path` with the writer thread parked on
    /// `gate` until the test releases it, so a save can be *provably* still
    /// in the channel when a forget lands on top of it (issue #131) instead
    /// of that interleaving depending on how fast the disk is.
    #[cfg(test)]
    fn with_gated_scene_bosses_path(self, path: PathBuf, gate: Receiver<()>) -> Self {
        self.with_scene_bosses_writer(path, Some(gate))
    }

    fn with_scene_bosses_writer(mut self, path: PathBuf, gate: Option<Receiver<()>>) -> Self {
        let cached = scene_bosses_cache::load(&path);
        self.meter.set_scene_bosses(cached);
        self.scene_bosses_writer = Some(CacheWriter::spawn_inner(path, gate));
        self
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

    /// Hands the current scene -> final-boss map to its background writer,
    /// if one is configured (issue #131). Same off-thread, never-panics
    /// contract as `save_names_cache`.
    pub fn save_scene_bosses(&self) {
        if let Some(writer) = &self.scene_bosses_writer {
            writer.save(self.meter.scene_bosses_for_save());
        }
    }

    /// Clears the learned scene -> final-boss map, both in-process and on
    /// disk (issue #131's "Forget learned bosses" menu action, wired from
    /// `ui.rs`'s `UiCommand::ForgetLearnedBosses`). The delete goes through
    /// the same background writer as the saves, so it can never be undone
    /// by a pre-forget snapshot that thread had not drained yet — and the
    /// in-process map is cleared first, so any save enqueued behind this
    /// one carries the cleared map (see `CacheWriter::send`).
    pub fn forget_scene_bosses(&mut self) {
        self.meter.set_scene_bosses(HashMap::new());
        if let Some(writer) = &self.scene_bosses_writer {
            writer.forget();
        }
    }

    /// Applies one protocol event. Returns `Some(reason)` when the event
    /// triggered a reset (boss-HP rollback, or the first hit of a new
    /// fight — including one on the far side of a `ServerChanged`
    /// reconnect, issue #138). A `ServerChanged` event itself never
    /// triggers a reset: it only invalidates entity/scene state and
    /// freezes the fight clock, leaving the displayed stats on screen.
    ///
    /// `now_ms` is supplied by the caller rather than read off the wall
    /// clock inside, so the whole pipeline stays deterministic and
    /// replayable (`crates/app/tests/`): events carrying no timestamp of
    /// their own — `ServerChanged` above all — are stamped with it.
    pub fn step(&mut self, ev: proto::ProtocolEvent, now_ms: u64) -> Option<meter::ResetReason> {
        let reason = self.meter.apply(&map_event(ev, now_ms));
        if let Some(reason) = reason {
            log::debug!("meter reset: {reason:?}");
            self.save_names_cache();
            self.save_scene_bosses();
        }
        reason
    }

    /// Manual reset, triggered by the overlay's Reset button.
    pub fn reset(&mut self, now_ms: u64) {
        self.meter.reset(meter::ResetReason::Manual, now_ms);
        self.save_names_cache();
        self.save_scene_bosses();
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

    /// Stops the background scene-bosses-writer thread, blocking until it
    /// has written any pending snapshot to disk (issue #131). Mirrors
    /// `shutdown_names_cache` exactly.
    pub fn shutdown_scene_bosses(&mut self) {
        if let Some(writer) = self.scene_bosses_writer.take() {
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
    scene_bosses_path: PathBuf,
) -> (Receiver<meter::Snapshot>, JoinHandle<()>) {
    let (tx_snapshot, rx_snapshot) = bounded::<meter::Snapshot>(1);
    let stale = rx_snapshot.clone();

    let handle = std::thread::Builder::new()
        .name("pipeline".to_string())
        .spawn(move || {
            run(
                events,
                commands,
                tx_snapshot,
                stale,
                names_cache_path,
                scene_bosses_path,
            )
        })
        .expect("failed to spawn the pipeline thread");

    (rx_snapshot, handle)
}

fn run(
    events: Receiver<proto::ProtocolEvent>,
    commands: Receiver<UiCommand>,
    tx_snapshot: Sender<meter::Snapshot>,
    stale: Receiver<meter::Snapshot>,
    names_cache_path: PathBuf,
    scene_bosses_path: PathBuf,
) {
    let mut pipeline =
        Pipeline::with_names_cache_path(names_cache_path).with_scene_bosses_path(scene_bosses_path);
    // Replaced by `never()` once capture disconnects, so a dead channel does
    // not spin the select loop.
    let mut events = events;
    let ticker = tick(TICK_INTERVAL);

    loop {
        select! {
            recv(events) -> msg => match msg {
                Ok(ev) => {
                    pipeline.step(ev, now_ms());
                }
                Err(_) => {
                    log::info!("capture channel closed; pipeline is idle");
                    events = crossbeam_channel::never();
                }
            },
            recv(commands) -> msg => match msg {
                Ok(UiCommand::Reset) => pipeline.reset(now_ms()),
                Ok(UiCommand::ForgetLearnedBosses) => pipeline.forget_scene_bosses(),
                Ok(UiCommand::Quit) => break,
                Err(_) => break,
            },
            recv(ticker) -> _ => publish(&mut pipeline, &tx_snapshot, &stale),
        }
    }

    // Shutdown save: catches identity data learned since the last
    // reset/encounter-end save (e.g. a session with no resets at all). Then
    // stop the writer threads, blocking until each has drained this (and
    // any still-pending) snapshot to disk, so the final save is never lost.
    pipeline.save_names_cache();
    pipeline.save_scene_bosses();
    pipeline.shutdown_names_cache();
    pipeline.shutdown_scene_bosses();
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
        let mapped = map_event(proto::ProtocolEvent::Damage(d.clone()), 0);
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
        let mapped = map_event(proto::ProtocolEvent::Damage(d), 0);
        let meter::ProtocolEvent::Damage(m) = mapped else {
            panic!("expected a damage event");
        };
        assert!(m.is_dead);
    }

    #[test]
    fn maps_player_info() {
        let mapped = map_event(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 42,
                name: Some("Foo".to_string()),
                class: Some(proto::Class::Marksman),
                ability_score: Some(9_999),
                season_level: Some(42),
                season_strength: Some(3_333),
                skill_ids: Vec::new(),
            }),
            0,
        );
        assert_eq!(
            mapped,
            meter::ProtocolEvent::Player(meter::PlayerInfo {
                uid: 42,
                name: Some("Foo".to_string()),
                class: Some(meter::Class::Marksman),
                ability_score: Some(9_999),
                season_strength: Some(3_333),
                imagines: None,
                imagine_tiers: None,
            })
        );
    }

    #[test]
    fn maps_player_info_classifies_skill_ids_into_imagine_slots() {
        // 3905 and 102640 are both the canonical Boar Imagine (see the spec's
        // "Many-to-one is expected" section); 3926 is a distinct Imagine.
        // 999_999_999 is not in the curated table and must not consume slot 2.
        // Tiers (issues #169/#170) are deliberately distinct per id so a
        // wrong-slot or wrong-source tier mixup would fail this assertion.
        let mapped = map_event(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 7,
                name: None,
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: None,
                skill_ids: vec![(3905, 1), (102640, 4), (3926, 3), (999_999_999, 9)],
            }),
            0,
        );
        let meter::ProtocolEvent::Player(p) = mapped else {
            panic!("expected a player event");
        };
        assert_eq!(p.imagines, Some([Some(3905), Some(3926)]));
        assert_eq!(p.imagine_tiers, Some([Some(1), Some(3)]));
    }

    #[test]
    fn maps_player_info_leaves_imagines_none_when_skill_ids_is_empty() {
        let mapped = map_event(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 8,
                name: None,
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: None,
                skill_ids: Vec::new(),
            }),
            0,
        );
        let meter::ProtocolEvent::Player(p) = mapped else {
            panic!("expected a player event");
        };
        assert_eq!(p.imagines, None);
        assert_eq!(p.imagine_tiers, None);
    }

    #[test]
    fn maps_enemy_hp() {
        let mapped = map_event(
            proto::ProtocolEvent::EnemyHp(proto::EnemyHp {
                uid: 10,
                curr_hp: Some(55),
                max_hp: Some(100),
                monster_id: Some(3),
                timestamp_ms: 1_234,
            }),
            0,
        );
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
        let mapped = map_event(proto::ProtocolEvent::Scene { level_map_id: 4242 }, 0);
        assert_eq!(mapped, meter::ProtocolEvent::Scene { level_map_id: 4242 });
    }

    #[test]
    fn maps_server_changed_stamps_the_scripted_timestamp() {
        let mapped = map_event(proto::ProtocolEvent::ServerChanged, 4_242);
        assert_eq!(
            mapped,
            meter::ProtocolEvent::ServerChanged {
                timestamp_ms: 4_242
            }
        );
    }

    #[test]
    fn damage_accumulates_into_the_snapshot() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)), 1_000);
        p.step(proto::ProtocolEvent::Damage(damage(2, 300, 1_000)), 1_000);
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
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)), 1_000);

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
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)), 1_000);
        p.tick(600_000);

        let reason = p.step(
            proto::ProtocolEvent::Damage(damage(1, 300, 600_000)),
            600_000,
        );
        assert_eq!(reason, Some(meter::ResetReason::NewFight));
        assert_eq!(p.snapshot(601_000).total_damage, 300);
        assert_eq!(p.tick(601_000), meter::FightState::Active);
    }

    #[test]
    fn manual_reset_clears_the_snapshot() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)), 1_000);
        p.reset(2_000);
        let snap = p.snapshot(3_000);
        assert_eq!(snap.total_damage, 0);
        assert!(snap.rows.is_empty());
    }

    /// issue #138: zoning/reconnecting must not wipe the numbers the
    /// player is still reading, so a `ServerChanged` event must not report
    /// a reset, must leave the accumulated stats on screen, and must freeze
    /// the fight clock at the moment of the reconnect rather than the
    /// caller's clock. `ServerChanged` carries no timestamp of its own, so
    /// `step`'s explicit `now_ms` is what stamps it — on the same
    /// controllable timeline the damage event below sits on.
    #[test]
    fn server_changed_keeps_the_snapshot_on_screen() {
        let mut p = Pipeline::new();
        let base = 1_000;
        p.step(proto::ProtocolEvent::Damage(damage(1, 700, base)), base);
        let reason = p.step(proto::ProtocolEvent::ServerChanged, base + 1_000);
        assert_eq!(reason, None);

        let snap = p.snapshot(base + 2_000);
        assert_eq!(snap.total_damage, 700);
        assert!(!snap.rows.is_empty());
        assert_eq!(
            snap.duration_ms, 1_000,
            "clock must freeze at the reconnect moment"
        );

        // The freeze must hold, not just happen to match at one snapshot
        // time: a later snapshot must read the exact same duration.
        let later = p.snapshot(base + 60_000);
        assert_eq!(
            later.duration_ms, 1_000,
            "duration must stay pinned while held"
        );
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
            p.step(proto::ProtocolEvent::Damage(damage(5, 100, 1_000)), 1_000);
            let snap = p.snapshot(2_000);
            assert_eq!(snap.rows[0].name, "Cached");

            p.shutdown_names_cache();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn manual_reset_persists_the_names_cache_to_disk() {
            let path = scratch_path("manual-reset");
            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(
                proto::ProtocolEvent::Player(proto::PlayerInfo {
                    uid: 1,
                    name: Some("Foo".to_string()),
                    class: None,
                    ability_score: None,
                    season_level: None,
                    season_strength: None,
                    skill_ids: Vec::new(),
                }),
                1_000,
            );

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
            // `step()` driving a reset (here, the next fight's first hit
            // after a held fight, `ResetReason::NewFight`) persists the
            // cache via the background writer, not just the manual-reset
            // path exercised above. `ServerChanged` can no longer cover
            // this branch: it deliberately never triggers a reset (issue
            // #138) — zoning must not wipe the numbers on screen.
            let path = scratch_path("step-reset");
            let mut p = Pipeline::with_names_cache_path(path.clone());
            p.step(
                proto::ProtocolEvent::Player(proto::PlayerInfo {
                    uid: 1,
                    name: Some("Foo".to_string()),
                    class: None,
                    ability_score: None,
                    season_level: None,
                    season_strength: None,
                    skill_ids: Vec::new(),
                }),
                1_000,
            );
            p.step(proto::ProtocolEvent::Damage(damage(1, 700, 1_000)), 1_000);
            p.tick(600_000);

            assert!(!path.exists());
            let reason = p.step(
                proto::ProtocolEvent::Damage(damage(1, 300, 600_000)),
                600_000,
            );
            assert_eq!(reason, Some(meter::ResetReason::NewFight));

            p.shutdown_names_cache();
            assert!(path.exists());

            let loaded = meter::names_cache::load(&path);
            assert_eq!(cached_name(&loaded, 1), Some("Foo".to_string()));

            let _ = std::fs::remove_file(&path);
        }
    }

    /// Issue #131: `Pipeline`-level wiring for the scene -> final-boss
    /// cache, mirroring `names_cache_wiring` above.
    mod scene_bosses_wiring {
        use super::*;
        use bpsr_test_support::scratch_path;

        fn boss_hit(uid: i64, ts: u64) -> proto::DamageEvent {
            proto::DamageEvent {
                attacker_uid: 1,
                attacker_kind: proto::EntityKind::Player,
                skill_id: 7,
                value: 1,
                crit: false,
                lucky: false,
                hp_lessen: 0,
                is_miss: false,
                is_heal: false,
                target_uid: uid,
                target_kind: proto::EntityKind::Monster,
                timestamp_ms: ts,
                is_dead: false,
            }
        }

        fn boss_hp(uid: i64, monster_id: u32, ts: u64) -> proto::EnemyHp {
            proto::EnemyHp {
                uid,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            }
        }

        #[test]
        fn with_scene_bosses_path_seeds_the_meter_from_an_existing_file() {
            // 1001 ("Tina's Mindrealm") is a dungeon scene; 103 ("Rathalos")
            // is a genuine boss.
            let names_path = scratch_path("scene-bosses-seed-names");
            let scene_path = scratch_path("scene-bosses-seed");
            scene_bosses_cache::save(&scene_path, &HashMap::from([(1001, 103)]));

            let mut p = Pipeline::with_names_cache_path(names_path.clone())
                .with_scene_bosses_path(scene_path.clone());
            p.step(proto::ProtocolEvent::Scene { level_map_id: 1001 }, 0);
            let snap = p.snapshot(1_000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));

            p.shutdown_names_cache();
            p.shutdown_scene_bosses();
            let _ = std::fs::remove_file(&names_path);
            let _ = std::fs::remove_file(&scene_path);
        }

        #[test]
        fn manual_reset_persists_the_scene_bosses_cache_to_disk() {
            let names_path = scratch_path("scene-bosses-manual-reset-names");
            let scene_path = scratch_path("scene-bosses-manual-reset");
            let mut p = Pipeline::with_names_cache_path(names_path.clone())
                .with_scene_bosses_path(scene_path.clone());
            p.step(proto::ProtocolEvent::Scene { level_map_id: 1001 }, 0);
            p.step(proto::ProtocolEvent::Damage(boss_hit(10, 0)), 0);
            p.step(proto::ProtocolEvent::EnemyHp(boss_hp(10, 103, 0)), 0);

            assert!(!scene_path.exists());
            p.reset(1_000);
            // The save is handed off to the background writer thread; flush
            // and join it before asserting the on-disk state.
            p.shutdown_scene_bosses();
            assert!(scene_path.exists());

            let loaded = scene_bosses_cache::load(&scene_path);
            assert_eq!(loaded.get(&1001), Some(&103));

            p.shutdown_names_cache();
            let _ = std::fs::remove_file(&names_path);
            let _ = std::fs::remove_file(&scene_path);
        }

        #[test]
        fn forget_learned_bosses_clears_in_process_state_and_deletes_the_file() {
            let names_path = scratch_path("scene-bosses-forget-names");
            let scene_path = scratch_path("scene-bosses-forget");
            scene_bosses_cache::save(&scene_path, &HashMap::from([(1001, 103)]));

            let mut p = Pipeline::with_names_cache_path(names_path.clone())
                .with_scene_bosses_path(scene_path.clone());
            p.step(proto::ProtocolEvent::Scene { level_map_id: 1001 }, 0);
            assert_eq!(
                p.snapshot(1_000).encounter.scene_boss_name,
                Some("Rathalos")
            );

            p.forget_scene_bosses();
            assert_eq!(p.snapshot(2_000).encounter.scene_boss_name, None);
            // The delete is handed off to the background writer thread, same
            // as the saves; flush and join it before asserting on disk.
            p.shutdown_scene_bosses();
            assert!(!scene_path.exists());

            p.shutdown_names_cache();
            let _ = std::fs::remove_file(&names_path);
        }

        /// The whole reason deletes ride the writer's channel instead of
        /// going out of band: a save the writer has not drained yet must
        /// never recreate the file a later forget removed.
        ///
        /// This pins the interleaving down rather than racing for it — the
        /// writer thread is parked on a gate for the entire enqueue
        /// sequence, so the save is *provably* still sitting in the channel
        /// when the forget lands on top of it.
        #[test]
        fn a_forget_is_not_undone_by_a_save_the_writer_has_not_drained_yet() {
            let names_path = scratch_path("scene-bosses-forget-race-names");
            let scene_path = scratch_path("scene-bosses-forget-race");
            scene_bosses_cache::save(&scene_path, &HashMap::from([(1001, 103)]));
            assert!(scene_path.exists());

            let (release, gate) = bounded::<()>(0);
            let mut p = Pipeline::with_names_cache_path(names_path.clone())
                .with_gated_scene_bosses_path(scene_path.clone(), gate);

            // Queue exactly the data the user is about to forget (the map
            // seeded from the file above), then forget it — both while the
            // writer thread cannot possibly have run.
            p.save_scene_bosses();
            p.forget_scene_bosses();

            // Release the writer, then block until it has drained.
            drop(release);
            p.shutdown_scene_bosses();
            assert!(!scene_path.exists());

            p.shutdown_names_cache();
            let _ = std::fs::remove_file(&names_path);
        }
    }

    #[test]
    fn player_info_names_a_row_that_already_has_damage() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(5, 100, 1_000)), 1_000);
        p.step(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 5,
                name: Some("Late".to_string()),
                class: Some(proto::Class::FrostMage),
                ability_score: None,
                season_level: None,
                season_strength: None,
                skill_ids: Vec::new(),
            }),
            1_000,
        );
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].name, "Late");
        assert_eq!(snap.rows[0].class, Some(meter::Class::FrostMage));
    }

    #[test]
    fn ability_score_flows_from_protocol_player_info_to_the_snapshot_row() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(9, 100, 1_000)), 1_000);
        p.step(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 9,
                name: None,
                class: None,
                ability_score: Some(77_000),
                season_level: None,
                season_strength: None,
                skill_ids: Vec::new(),
            }),
            1_000,
        );
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].ability_score, Some(77_000));
    }

    #[test]
    fn season_strength_flows_from_protocol_player_info_to_the_snapshot_row() {
        let mut p = Pipeline::new();
        p.step(proto::ProtocolEvent::Damage(damage(10, 100, 1_000)), 1_000);
        p.step(
            proto::ProtocolEvent::Player(proto::PlayerInfo {
                uid: 10,
                name: None,
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: Some(3_333),
                skill_ids: Vec::new(),
            }),
            1_000,
        );
        let snap = p.snapshot(2_000);
        assert_eq!(snap.rows[0].season_strength, Some(3_333));
    }

    mod imagine_slots_classification {
        use super::*;

        /// `ids` with an implicit tier of 0 each — for tests that only care
        /// about id classification, not tier.
        fn ids_only(ids: &[i32]) -> Vec<(i32, i32)> {
            ids.iter().map(|&id| (id, 0)).collect()
        }

        #[test]
        fn two_distinct_imagines_fill_both_slots_in_wire_order() {
            assert_eq!(
                imagine_slots(&ids_only(&[3926, 3905])),
                ([Some(3926), Some(3905)], [Some(0), Some(0)])
            );
        }

        #[test]
        fn the_boar_variant_group_dedups_to_one_slot_and_frees_the_second() {
            // 102651 ("Boar Knight") and 102655 ("Boar Impact!") have raw
            // names that diverge from the canonical 3905 ("Stunt! Boarrier
            // Rush"), so this only passes if dedup is keyed on the resolved
            // `Imagine` (== icon), not on `name` or raw skill id.
            let ids = ids_only(&[3905, 102640, 102651, 102655, 102658, 3926]);
            assert_eq!(imagine_slots(&ids).0, [Some(3905), Some(3926)]);
        }

        #[test]
        fn a_name_divergent_variant_still_collapses_with_its_canonical_pair() {
            // 2002840 ("Arcane! Swift Vortex") is a variant of the canonical
            // 3926 ("Arcane! Meteor Shower") and shares its icon.
            assert_eq!(
                imagine_slots(&ids_only(&[3926, 2002840])).0,
                [Some(3926), None]
            );
        }

        #[test]
        fn unknown_ids_are_skipped_and_never_consume_a_slot() {
            assert_eq!(
                imagine_slots(&ids_only(&[999_999_999, 3905, -1, 3926])).0,
                [Some(3905), Some(3926)]
            );
        }

        #[test]
        fn more_than_two_known_imagines_truncates_to_the_first_two() {
            // 3905, 3926, and a third distinct Imagine (3901) — only the
            // first two distinct Imagines seen should occupy slots.
            assert_eq!(
                imagine_slots(&ids_only(&[3905, 3926, 3901])).0,
                [Some(3905), Some(3926)]
            );
        }

        #[test]
        fn an_empty_list_yields_two_empty_slots() {
            assert_eq!(imagine_slots(&[]), ([None, None], [None, None]));
        }

        // -- Tier narrowing (issues #169/#170) -------------------------------

        /// Each equipped slot must carry the tier that was paired with its
        /// own representative skill id in the wire-order list, not some
        /// other slot's tier or an unrelated default.
        #[test]
        fn each_slot_carries_its_own_equipped_ids_tier() {
            assert_eq!(
                imagine_slots(&[(3905, 2), (3926, 5)]),
                ([Some(3905), Some(3926)], [Some(2), Some(5)])
            );
        }

        /// Dedup keeps the *first* occurrence of a many-to-one group's
        /// representative id (see `the_boar_variant_group_dedups_...`
        /// above) — its tier must come from that same first occurrence,
        /// not a later variant's.
        #[test]
        fn dedup_keeps_the_first_seen_variants_tier() {
            // 102640 is a Boar variant of the canonical 3905.
            assert_eq!(
                imagine_slots(&[(3905, 1), (102640, 4)]),
                ([Some(3905), None], [Some(1), None])
            );
        }

        #[test]
        fn a_zero_tier_is_kept_as_a_real_value_not_dropped() {
            assert_eq!(
                imagine_slots(&[(3905, 0)]),
                ([Some(3905), None], [Some(0), None])
            );
        }
    }
}
