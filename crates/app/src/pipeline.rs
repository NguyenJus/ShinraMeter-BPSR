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

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bpsr_capture::CaptureRestart;
use bpsr_meter as meter;
use bpsr_protocol as proto;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, tick};

use crate::history::{self, writer::HistoryHandle};
use crate::imagines;
use crate::ui::{UiCommand, encounter_subtitle, encounter_title};

/// Snapshot publication rate (~10 Hz) — a ceiling on how often `publish`
/// re-evaluates the meter, not the overlay's repaint cadence: issue #349
/// made the overlay wake on its own (via [`RepaintHandle`]) the moment a
/// *changed* snapshot lands, rather than polling this channel on a fixed
/// clock.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// A handle `publish` can use to wake the overlay's egui event loop the
/// moment a changed snapshot is ready, instead of relying on the UI thread
/// to notice on its own next scheduled repaint (issue #349's root cause:
/// nothing on the pipeline side ever called `request_repaint`, so the
/// overlay only picked up a fresh snapshot once a second via the idle
/// heartbeat).
///
/// Backed by an `Arc<OnceLock<egui::Context>>` rather than a plain
/// `egui::Context` because the pipeline thread is spawned (and needs a
/// handle to close over) before `eframe::run_native`'s window-creation
/// closure has run and produced the real `Context` — the `OnceLock` is
/// filled in from that closure once it does. Every clone shares the same
/// cell, so filling it anywhere makes every existing handle live.
#[derive(Clone, Default)]
pub struct RepaintHandle(Arc<OnceLock<egui::Context>>);

impl RepaintHandle {
    /// A handle with no `egui::Context` behind it yet — used by
    /// `main.rs` before the window exists, and by tests that never create
    /// one at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fills the handle with the real context, once `eframe::run_native`'s
    /// creator closure has one. A no-op (via `OnceLock::set`'s `Err`) if
    /// called twice; the pipeline thread only ever reads through `wake`, so
    /// a second call finding the cell already full is harmless.
    pub fn install(&self, ctx: egui::Context) {
        let _ = self.0.set(ctx);
    }

    /// Asks egui to repaint immediately, if a `Context` has been installed.
    /// Silently does nothing before `install` runs (early snapshots, or the
    /// window failed to open) and in tests that never install one.
    fn wake(&self) {
        if let Some(ctx) = self.0.get() {
            ctx.request_repaint();
        }
    }
}

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
/// background writer carries, and how to persist it. A zero-sized marker
/// type per cache, so [`CacheWriter`] stays a single implementation however
/// many caches there are. (Issue #131 added a second one — the scene ->
/// final-boss cache — which issue #201 then removed in favour of the curated
/// `bpsr_meter::tables::SCENE_FINAL_BOSSES`; the shape is kept because it is
/// the cheaper thing to reuse than to re-derive.)
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

/// Persists one cache off the pipeline thread, so a slow disk can never
/// stall the `select!` loop that drains the bounded capture-event channel.
/// The channel has capacity 1 and coalesces: a command still waiting to be
/// carried out is dropped in favour of a newer one rather than queued,
/// since only the latest state of the file is ever worth reaching.
struct CacheWriter<P: CachePersist> {
    tx: Sender<P::Snapshot>,
    /// A second receiver handle used only to drain a stale, not-yet-written
    /// snapshot out of the channel before enqueuing a newer one (mirrors
    /// `publish`'s drop-oldest pattern for UI snapshots below).
    stale: Receiver<P::Snapshot>,
    handle: Option<JoinHandle<()>>,
}

impl<P: CachePersist> CacheWriter<P> {
    fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = bounded::<P::Snapshot>(1);
        let stale = rx.clone();

        let handle = std::thread::Builder::new()
            .name(P::THREAD_NAME.to_string())
            .spawn(move || {
                // Keeps draining until every `Sender` (the pipeline's `tx`
                // plus `stale`, once `CacheWriter` is dropped) is gone *and*
                // the channel is empty — so a snapshot enqueued right before
                // shutdown is still written before this thread exits.
                while let Ok(snapshot) = rx.recv() {
                    P::save(&path, &snapshot);
                }
            })
            .unwrap_or_else(|err| panic!("failed to spawn the {} thread: {err}", P::THREAD_NAME));

        Self {
            tx,
            stale,
            handle: Some(handle),
        }
    }

    /// Enqueues `snapshot` to be written, without ever blocking on the
    /// writer thread: when the capacity-1 channel is full, the snapshot
    /// still sitting in it is drained and this newer one takes its place.
    ///
    /// Discarding that pending snapshot can never change what ends up on
    /// disk: each one states the file's next contents outright rather than
    /// amending them (`save` rewrites the whole file), so a snapshot
    /// superseded by a newer one is stale by definition — only the latest
    /// cache state is worth persisting.
    fn save(&self, msg: P::Snapshot) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(msg)) => {
                let _ = self.stale.try_recv();
                let _ = self.tx.try_send(msg);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Closes the channel and blocks until the writer thread has written any
    /// pending snapshot. Must be called before process exit so the final
    /// save is never lost.
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
    /// The history thread's handle (issue #39), or `None` in tests and
    /// `Pipeline::new()` — no handle means no history writes at all, the
    /// same "`None` means no disk IO" contract `cache_writer` follows.
    history: Option<HistoryHandle>,
    /// Issue #39's write-exactly-once latch: `true` once the current
    /// `FightState::Ended` has been recorded, cleared the moment the state
    /// leaves `Ended`. Mirrors `Meter::latch_fight_end`'s idempotency shape
    /// (`crates/meter/src/encounter.rs`) — a nine-second post-fight freeze
    /// produces ~90 consecutive `Ended` ticks and must produce one row.
    fight_end_recorded: bool,
    /// The record `record_fight_end` captured when the current fight ended,
    /// held back until `Meter::fight_config`'s `post_end_grace_ms` window
    /// has closed on it (issue #post-end-grace).
    ///
    /// Needed because the meter keeps folding trailing packets into an
    /// ended fight's stats for that whole window
    /// (`bpsr_meter::encounter::Meter::apply_damage_grace`), but the very
    /// first `Ended` tick used to record history immediately — racing every
    /// one of them. The record actually sent is rebuilt at the moment the
    /// window closes, so it carries those packets; this cached copy exists
    /// for the case where the hold ends *early* — a scene change, a manual
    /// reset, or the next pull's first hit landing inside the window —
    /// after which the meter no longer holds the fight's rows at all and
    /// this is the only copy left. `None` once flushed, once discarded (see
    /// `settle_pending_fight_end`), or if the fight had no history worth
    /// building.
    pending_fight_end: Option<history::EncounterRecord>,
    /// `Meter::fight_start_ms` as it was when `pending_fight_end` was
    /// captured, and `None` whenever nothing is pending.
    ///
    /// The one signal that separates "the held fight resumed" (issue #124's
    /// phase resume, which keeps the fight clock and therefore this value)
    /// from "a new fight started, or the meter was reset" (both of which
    /// move or clear it) on the `Ended -> non-Ended` edge. See
    /// `record_fight_end`'s "Leaving `Ended` early".
    held_fight_start_ms: Option<u64>,
    /// Pipeline-robustness audit, finding 1: `true` until `run`'s events
    /// channel disconnects (the capture thread panicked or exited and
    /// dropped `tx_events`), `false` for the rest of the session after.
    /// Stamped onto every `Snapshot` this publishes from then on
    /// (`snapshot_focused`, below) — the snapshot channel itself never
    /// disconnects in this scenario, so without this the overlay has no way
    /// to tell "capture is quiet because nothing is happening" apart from
    /// "capture is gone and this will never change again."
    capture_alive: bool,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            meter: meter::Meter::new(),
            cache_writer: None,
            history: None,
            fight_end_recorded: false,
            pending_fight_end: None,
            held_fight_start_ms: None,
            capture_alive: true,
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
            history: None,
            fight_end_recorded: false,
            pending_fight_end: None,
            held_fight_start_ms: None,
            capture_alive: true,
        }
    }

    /// Attaches the history thread's handle (issue #39). Takes `self` by
    /// value so it chains after the cache constructor.
    pub fn with_history(mut self, history: HistoryHandle) -> Self {
        self.history = Some(history);
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

    /// Applies one protocol event. Returns `Some(reason)` when the event
    /// triggered a reset: boss-HP rollback, the first hit of a new fight —
    /// including one on the far side of a `ServerChanged` reconnect, issue
    /// #138 — or `Scene` resolving to a dungeon/raid id different from the
    /// one already held (issue #191). A `ServerChanged` event itself never
    /// triggers a reset: it carries no destination scene id, so it only
    /// invalidates entity/scene state and freezes the fight clock, leaving
    /// the displayed stats on screen for the `Scene` event that follows to
    /// judge once the destination is known.
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
        }
        // Pipeline-robustness audit, finding 3: `record_fight_end` used to
        // run only from `publish`'s 100ms ticker, so a fight that both ended
        // (e.g. a boss death, which `Meter::apply` latches synchronously)
        // and was reset by a new pull's first hit inside the same tick
        // window was never written to history — the ticker's next call saw
        // `FightState::Active` and the `Ended` tick in between never
        // happened. Idempotent via `fight_end_recorded`, so calling it here
        // too costs nothing on every other event; it only ever writes once
        // per ended fight, same as the ticker-driven call in `publish`.
        self.record_fight_end(self.meter.fight_state(now_ms), now_ms);
        reason
    }

    /// Manual reset, triggered by the overlay's Reset button.
    pub fn reset(&mut self, now_ms: u64) {
        self.meter.reset(meter::ResetReason::Manual, now_ms);
        self.save_names_cache();
    }

    /// Pipeline-robustness audit, finding 1: called from `run`'s events arm
    /// once the capture-event channel disconnects. Latches — nothing in
    /// this process restarts the capture thread — so every `Snapshot`
    /// published from here on (`snapshot_focused`, below) carries
    /// `capture_alive: false` for the rest of the session, which is the
    /// overlay's only signal that the meter is now permanently frozen (see
    /// `ui::OverlayApp::raise_capture_dead_status`).
    fn mark_capture_dead(&mut self) {
        self.capture_alive = false;
    }

    pub fn snapshot(&self, now_ms: u64) -> meter::Snapshot {
        self.meter.snapshot(now_ms)
    }

    /// The live publish loop's snapshot call (PR #268 review, finding 2):
    /// only the players named in `skill_focus` get their heals/dealt/
    /// received/casts breakdowns built, since a skill window is closed
    /// almost all the time. See `Meter::snapshot_focused`'s doc comment.
    /// Every other caller (tests, replay/history, the sanitizer) keeps
    /// using `snapshot` above, unaffected.
    ///
    /// Stamps `capture_alive` (finding 1) onto the snapshot the meter built:
    /// the meter has no notion of capture, so this — the one path every
    /// live-published snapshot goes through — is where that fact has to be
    /// attached.
    fn snapshot_focused(&self, now_ms: u64, skill_focus: &[i64]) -> meter::Snapshot {
        let mut snapshot = self.meter.snapshot_focused(now_ms, Some(skill_focus));
        snapshot.capture_alive = self.capture_alive;
        snapshot
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

    /// Records the encounter that just ended, exactly once (issue #39).
    ///
    /// Called from `publish` immediately after `tick`/`snapshot`, on the
    /// `Active -> Ended` edge — deliberately *not* from `Meter::reset`, which
    /// has already cleared `players` by the time any caller learns a fight is
    /// over.
    ///
    /// Cheap on this thread: it clones the snapshot's rows into owned DTOs and
    /// enqueues them on an unbounded channel, then returns. Every failure past
    /// that point is the history thread's to log and swallow.
    ///
    /// Issue #post-end-grace: does **not** send to history on the first
    /// `Ended` tick any more. `Meter::apply` keeps folding trailing packets
    /// into an ended fight's stats for
    /// `Meter::fight_config().post_end_grace_ms` after `fight_end_ms` — if
    /// this sent immediately, the saved row would race that window and
    /// routinely miss the tail of the fight it is supposed to capture (the
    /// very packets the grace window exists to keep). So nothing goes out
    /// until the window has closed, and the record that does go out is
    /// built *then*, from a meter that has already absorbed every trailing
    /// packet. Chosen over re-recording an already-sent row
    /// (`crates/app/src/history` has no update-by-id request, only
    /// `Record`/`Delete`/`Clear`) as the simpler of the two options the
    /// grace window's design left open.
    ///
    /// # Window boundary
    ///
    /// The window counts as closed once `now_ms - ended_at_ms` is
    /// **strictly greater than** `post_end_grace_ms`, because the meter's
    /// own test (`bpsr_meter::encounter::Meter::in_post_end_grace_window`)
    /// is inclusive: a packet stamped exactly `ended_at_ms +
    /// post_end_grace_ms` still gets folded in. Flushing at `==` would race
    /// that last millisecond — precisely the bug this defers for. The two
    /// sides share one convention deliberately; the meter's doc comment
    /// points back here.
    ///
    /// # Cost
    ///
    /// One `Meter::snapshot` per fight end in the common case: an
    /// idle-timeout end is already `idle_timeout_ms` (9s stock) past
    /// `fight_end_ms` the first tick it is observed, i.e. long past the 2s
    /// grace window, so it takes the flush path straight away. At most two
    /// for a boss-death end genuinely observed inside the window — one to
    /// capture the pending record, one to rebuild it when the window
    /// closes. Every tick in between does no snapshot work at all, and a
    /// pipeline with no history handle (`Pipeline::new()`, most tests)
    /// returns before touching the meter (PR #333 review, finding 1).
    ///
    /// # Leaving `Ended` early
    ///
    /// Two very different things flip the state away from `Ended` before
    /// the window closes, and `settle_pending_fight_end` tells them apart
    /// by `Meter::fight_start_ms`:
    ///
    /// * **The held fight resumed.** A phase-2 boss taking its first hit
    ///   clears `fight_end_ms` and keeps the fight clock running
    ///   (`bpsr_meter::encounter::Meter::resumes_held_fight`, issue #124):
    ///   the *same* fight is still going, so `fight_start_ms` is unchanged.
    ///   The pending record covers phase 1 only, so sending it would write
    ///   a truncated row now and a second, complete row when the fight
    ///   really ends. It is discarded instead (PR #333 review, finding 2).
    /// * **A new fight started, or the meter was reset.** `fight_start_ms`
    ///   moved (a `ResetReason::NewFight`) or went away (a scene change or
    ///   manual reset). The old fight really is over and the meter no
    ///   longer holds its rows, so the pending record is the only copy left
    ///   — it gets flushed rather than dropped.
    ///
    /// A resume landing *after* the record has already gone out is out of
    /// scope here (stock `phase_resume_window_ms` is 60s against a 2s
    /// grace, so that ordering is the common one): un-writing a row would
    /// need an update-by-id request the history thread does not have.
    pub fn record_fight_end(&mut self, state: meter::FightState, now_ms: u64) {
        // No history handle means no history writes at all, so skip the
        // snapshot work rather than building records ~10 times a second for
        // `flush_pending_fight_end` to throw away.
        if self.history.is_none() {
            return;
        }
        if state != meter::FightState::Ended {
            self.settle_pending_fight_end();
            self.fight_end_recorded = false;
            return;
        }
        if self.fight_end_recorded {
            return;
        }
        // PR #329 review, finding 1: `fight_end_recorded` is only set once a
        // record actually goes out (the flush path below), never here.
        // `Meter::fight_state` computes the idle-timeout end on the fly
        // without latching it (only `Meter::tick` calls `latch_fight_end`),
        // so `step`'s call above can observe `Ended` while `fight_end_ms` is
        // still `None`; latching eagerly on that observation would poison
        // this flag for good. Leaving it clear here makes an unlatched
        // `Ended` a plain no-op that retries on the next call — by which
        // time `tick` has latched the end and the grace-window capture
        // below runs instead. The boss-death path is unaffected: `Meter::
        // apply` latches `fight_end_ms` synchronously, so it is already
        // `Some` by the time `step` looks.
        let Some(ended_at_ms) = self.meter.fight_end_ms() else {
            return;
        };
        let grace_ms = self.meter.fight_config().post_end_grace_ms;
        if now_ms.saturating_sub(ended_at_ms) <= grace_ms {
            // Still inside the window. Capture the fight exactly once, so a
            // hold cut short before the window closes still has something
            // to flush, then leave every later tick alone.
            if self.held_fight_start_ms.is_none() {
                self.held_fight_start_ms = self.meter.fight_start_ms();
                self.pending_fight_end = self.build_fight_end_record(now_ms, ended_at_ms);
            }
            return;
        }
        self.fight_end_recorded = true;
        self.held_fight_start_ms = None;
        self.pending_fight_end = self.build_fight_end_record(now_ms, ended_at_ms);
        self.flush_pending_fight_end();
    }

    /// Builds the history record for a fight that ended at `ended_at_ms`,
    /// or `None` when there is nothing worth saving (no rows, no damage).
    ///
    /// Builds its own full (never `skill_focus`-gated) snapshot rather than
    /// taking one from the caller (PR #268 review, finding 2): the live
    /// publish loop's own snapshot may have skipped the
    /// heals/dealt/received/casts breakdown for players with no skill
    /// window open, and a saved history record must never carry that gap.
    fn build_fight_end_record(
        &self,
        now_ms: u64,
        ended_at_ms: u64,
    ) -> Option<history::EncounterRecord> {
        let snapshot = self.meter.snapshot(now_ms);
        let title = encounter_title(&snapshot.encounter);
        let subtitle = encounter_subtitle(&snapshot.encounter);
        history::record_from_snapshot(&snapshot, ended_at_ms, title, subtitle)
    }

    /// Decides what happens to `pending_fight_end` when the state leaves
    /// `FightState::Ended` before the grace window closed: flushed if the
    /// fight is genuinely over, discarded if the very same fight resumed.
    /// See `record_fight_end`'s "Leaving `Ended` early" for why those two
    /// must not be treated alike.
    fn settle_pending_fight_end(&mut self) {
        let resumed = self
            .held_fight_start_ms
            .take()
            .is_some_and(|started_at| self.meter.fight_start_ms() == Some(started_at));
        if resumed {
            self.pending_fight_end = None;
        } else {
            self.flush_pending_fight_end();
        }
    }

    /// Sends `pending_fight_end` to history, if there is one queued and it
    /// has not already gone out. See `record_fight_end`'s doc comment for
    /// why the send is decoupled from the build.
    fn flush_pending_fight_end(&mut self) {
        let Some(record) = self.pending_fight_end.take() else {
            return;
        };
        if let Some(history) = &self.history {
            history.record(record);
        }
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
    history: Option<HistoryHandle>,
    // Issue #214: the `Send`-able half of the running capture, or `None`
    // when capture never started. This thread owns the UI's command
    // channel, so it is where `UiCommand::RestartCapture` has to land — the
    // `CaptureHandle` itself is pinned to `main`'s thread.
    capture_restart: Option<CaptureRestart>,
    // Issue #349: wakes the overlay's event loop the moment a changed
    // snapshot is published, rather than leaving it to notice on its own
    // next scheduled repaint. `main.rs` creates this before spawning (the
    // real `egui::Context` does not exist yet) and installs the context
    // once `eframe::run_native`'s creator closure runs; tests pass a fresh,
    // never-installed handle, in which case `publish`'s wake is a no-op.
    repaint: RepaintHandle,
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
                history,
                capture_restart,
                repaint,
            )
        })
        .expect("failed to spawn the pipeline thread");

    (rx_snapshot, handle)
}

/// What `run`'s loop should do after a `UiCommand` has been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandOutcome {
    Continue,
    Quit,
}

/// Applies one `UiCommand`. Factored out of `run`'s select loop (PR #329
/// review, finding 2) because the events-`Err` arm has to drain the very
/// same commands looking for a queued `Quit`, and the ones it passes on the
/// way — a `SkillFocus` the overlay sent just before closing, say — must
/// still take effect rather than be thrown away.
fn handle_command(
    cmd: UiCommand,
    pipeline: &mut Pipeline,
    skill_focus: &mut Vec<i64>,
    capture_restart: &Option<CaptureRestart>,
) -> CommandOutcome {
    match cmd {
        UiCommand::Reset => pipeline.reset(now_ms()),
        UiCommand::SkillFocus(uids) => *skill_focus = uids,
        // Issue #214. Logged unconditionally: knowing the user had to reach
        // for this — and when — is exactly the context #211's silent log was
        // missing, and it is what makes the capture-side lines that follow
        // interpretable.
        UiCommand::RestartCapture => match capture_restart {
            Some(restart) => {
                log::info!("restarting packet capture at the user's request");
                restart.request();
            }
            None => log::warn!(
                "a packet-capture restart was requested, but capture never started \
                 (the status banner explains why); nothing to restart"
            ),
        },
        UiCommand::Quit => return CommandOutcome::Quit,
    }
    CommandOutcome::Continue
}

/// Applies every command already queued and reports whether one of them was
/// a `Quit` (PR #329 review, finding 2). `run`'s events-`Err` arm calls this
/// to tell an orderly shutdown — `main.rs` queues `Quit` before it stops
/// capture, so the disconnect it is looking at was caused by the quit — from
/// the capture thread genuinely dying mid-session. Stops at the `Quit`: any
/// command behind it is moot, since the loop is about to exit.
fn drain_for_quit(
    commands: &Receiver<UiCommand>,
    pipeline: &mut Pipeline,
    skill_focus: &mut Vec<i64>,
    capture_restart: &Option<CaptureRestart>,
) -> bool {
    while let Ok(cmd) = commands.try_recv() {
        if handle_command(cmd, pipeline, skill_focus, capture_restart) == CommandOutcome::Quit {
            return true;
        }
    }
    false
}

// The pipeline thread's body: one private, single-call-site function whose
// arguments are simply the eight channel/handle ends the thread owns, so
// bundling them into a struct would only move the same list one indirection
// away (issue #349 added the eighth, `repaint`).
#[allow(clippy::too_many_arguments)]
fn run(
    events: Receiver<proto::ProtocolEvent>,
    commands: Receiver<UiCommand>,
    tx_snapshot: Sender<meter::Snapshot>,
    stale: Receiver<meter::Snapshot>,
    names_cache_path: PathBuf,
    history: Option<HistoryHandle>,
    capture_restart: Option<CaptureRestart>,
    repaint: RepaintHandle,
) {
    let mut pipeline = Pipeline::with_names_cache_path(names_cache_path);
    if let Some(history) = history {
        pipeline = pipeline.with_history(history);
    }
    // Replaced by `never()` once capture disconnects, so a dead channel does
    // not spin the select loop.
    let mut events = events;
    let ticker = tick(TICK_INTERVAL);
    // Which players have a skill-breakdown window open right now, per the
    // overlay's latest `UiCommand::SkillFocus` (PR #268 review, finding 2).
    // Read by `publish` below; empty until the UI has sent one, which just
    // means the first tick or two after startup builds no breakdowns —
    // corrected within one 100ms tick once the UI's first frame runs.
    let mut skill_focus: Vec<i64> = Vec::new();
    // Issue #349: the last snapshot `publish` sent, so it can tell a
    // genuinely new value (worth waking the overlay for) from a re-publish
    // of the same one (a tick where nothing changed — the overwhelmingly
    // common case while the game sits idle).
    let mut last_published: Option<meter::Snapshot> = None;

    loop {
        select! {
            recv(events) -> msg => match msg {
                Ok(ev) => {
                    pipeline.step(ev, now_ms());
                }
                Err(_) => {
                    // PR #329 review, finding 2: an ordinary shutdown
                    // reaches this arm too. `main.rs` stops capture (which
                    // joins the capture thread and so drops `tx_events`) as
                    // part of quitting, so without this check every clean
                    // exit logged the ERROR below and raised the overlay's
                    // dead-capture banner. `main.rs` now queues
                    // `UiCommand::Quit` *before* stopping capture, so a
                    // `Quit` already sitting in `commands` is the reliable
                    // "this disconnect was expected" signal — draining for
                    // it here (rather than trusting `select!` to pick the
                    // commands arm first, which it chooses at random among
                    // ready operations) is what makes the distinction
                    // race-free. Mirrors PR #326's UI-side `quit_requested`
                    // flag, which tells the same two cases apart on the
                    // snapshot channel.
                    if drain_for_quit(
                        &commands,
                        &mut pipeline,
                        &mut skill_focus,
                        &capture_restart,
                    ) {
                        // Issue #321: flush any fight already sitting in
                        // `FightState::Ended` before the thread exits, same
                        // as the commands arm below.
                        publish(&mut pipeline, &tx_snapshot, &stale, &skill_focus, &repaint, &mut last_published);
                        log::info!(
                            "capture channel closed after a quit was requested; this is an \
                             orderly shutdown"
                        );
                        break;
                    }
                    // Pipeline-robustness audit, finding 1: the capture
                    // thread panicked or exited and dropped `tx_events`.
                    // This used to log at `info` and otherwise carry on
                    // publishing snapshots forever — the snapshot channel
                    // never disconnects in this scenario, so the overlay
                    // had no way to tell the meter had gone permanently
                    // silent. `mark_capture_dead` stamps that fact onto
                    // every snapshot from here on so the UI can raise its
                    // own persistent banner (see
                    // `ui::OverlayApp::raise_capture_dead_status`).
                    log::error!(
                        "capture channel closed (the capture thread is gone); the pipeline will \
                         keep publishing snapshots but they will never change again for the rest \
                         of this session"
                    );
                    pipeline.mark_capture_dead();
                    events = crossbeam_channel::never();
                }
            },
            recv(commands) -> msg => {
                // Issue #321: a fight already sitting in `FightState::Ended`
                // at quit time would otherwise never reach history —
                // `record_fight_end` only ever runs from the tick arm
                // above, and quitting drops `tx_snapshot` (and with it the
                // UI's only signal) the moment this loop exits, with no
                // more ticks left to catch it. One last `publish` below
                // flushes that final state — and its `record_fight_end`
                // call — before the thread actually exits. Logged at INFO,
                // not the ERROR `ui/mod.rs::raise_pipeline_dead_status` used to
                // log for every orderly quit (issue #321's false positive):
                // this is the pipeline thread's own confirmation that the
                // shutdown it is about to cause was requested, not a crash.
                //
                // The overlay window closing without going through
                // `UiCommand::Quit` first (or any other drop of the
                // command channel) is an orderly shutdown too — see this
                // function's own doc comment — so it gets the same final
                // flush and the same INFO-level line.
                let quit_reason = match msg {
                    Ok(cmd) => {
                        if handle_command(cmd, &mut pipeline, &mut skill_focus, &capture_restart)
                            == CommandOutcome::Quit
                        {
                            Some(
                                "quit requested; pipeline flushed its final snapshot and is shutting down",
                            )
                        } else {
                            None
                        }
                    }
                    Err(_) => Some(
                        "command channel disconnected; pipeline flushed its final snapshot and is shutting down",
                    ),
                };
                if let Some(reason) = quit_reason {
                    publish(&mut pipeline, &tx_snapshot, &stale, &skill_focus, &repaint, &mut last_published);
                    log::info!("{reason}");
                    break;
                }
            },
            recv(ticker) -> _ => publish(&mut pipeline, &tx_snapshot, &stale, &skill_focus, &repaint, &mut last_published),
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
    skill_focus: &[i64],
    // Issue #349: wakes the overlay the moment a *changed* snapshot lands,
    // rather than leaving it to notice on its own next scheduled repaint.
    repaint: &RepaintHandle,
    // The last snapshot this function sent, so a re-publish of an unchanged
    // value (the overwhelmingly common case while the game sits idle) does
    // not needlessly wake the overlay.
    last_published: &mut Option<meter::Snapshot>,
) {
    // One `now` for the whole tick: the fight-state advance and the snapshot
    // it feeds must agree on what time it is.
    let now = now_ms();
    let state = pipeline.tick(now);
    let snap = pipeline.snapshot_focused(now, skill_focus);
    pipeline.record_fight_end(state, now);
    if last_published.as_ref() != Some(&snap) {
        repaint.wake();
        *last_published = Some(snap.clone());
    }
    if tx_snapshot.try_send(snap).is_err() {
        let _ = stale.try_recv();
        let _ = tx_snapshot.try_send(pipeline.snapshot_focused(now, skill_focus));
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
            kind: proto::DamageKind::Normal,
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
                position: None,
                target_position: None,
                shield: None,
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
                shield: None,
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
                position: None,
                target_position: None,
                shield: None,
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
                position: None,
                target_position: None,
                shield: None,
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
                position: None,
                target_position: None,
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

    /// Issue #139's three (now four) dungeon events are pure pass-through
    /// mappings, but the pass-through is exactly what has to hold: the
    /// meter's dungeon-state machine reads nothing else.
    #[test]
    fn maps_dungeon_state_event() {
        let mapped = map_event(
            proto::ProtocolEvent::DungeonState {
                state: proto::event::EDungeonState::Playing,
                scene_uuid: Some(4_242),
            },
            0,
        );
        assert_eq!(
            mapped,
            meter::ProtocolEvent::DungeonState {
                state: meter::EDungeonState::Playing,
                scene_uuid: Some(4_242),
            }
        );
    }

    #[test]
    fn maps_dungeon_objective_event() {
        let mapped = map_event(
            proto::ProtocolEvent::DungeonObjective {
                target_id: 1083,
                nums: Some(0),
                complete: Some(false),
            },
            0,
        );
        assert_eq!(
            mapped,
            meter::ProtocolEvent::DungeonObjective {
                target_id: 1083,
                nums: Some(0),
                complete: Some(false),
            }
        );
    }

    #[test]
    fn maps_dungeon_objective_removed_event() {
        let mapped = map_event(
            proto::ProtocolEvent::DungeonObjectiveRemoved { target_id: 1083 },
            0,
        );
        assert_eq!(
            mapped,
            meter::ProtocolEvent::DungeonObjectiveRemoved { target_id: 1083 }
        );
    }

    #[test]
    fn maps_dungeon_var_event() {
        let mapped = map_event(
            proto::ProtocolEvent::DungeonVar {
                name: "IsFinishTarget".to_string(),
                value: 1,
            },
            0,
        );
        assert_eq!(
            mapped,
            meter::ProtocolEvent::DungeonVar {
                name: "IsFinishTarget".to_string(),
                value: 1,
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
                    position: None,
                    target_position: None,
                    shield: None,
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
                    position: None,
                    target_position: None,
                    shield: None,
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
                position: None,
                target_position: None,
                shield: None,
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
                position: None,
                target_position: None,
                shield: None,
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
                position: None,
                target_position: None,
                shield: None,
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

    /// Issue #39: `record_fight_end`'s `Active -> Ended` edge trigger and its
    /// write-exactly-once latch.
    mod history_recording {
        use super::*;
        use crate::history::temp_history_path;
        use crate::history::writer::HistoryEvent;

        /// Drives one damage hit, then ticks past the idle timeout so the
        /// fight latches `FightState::Ended` — the edge `record_fight_end`
        /// is looking for.
        fn ended_snapshot(pipeline: &mut Pipeline) -> (meter::FightState, u64) {
            pipeline.step(proto::ProtocolEvent::Damage(damage(1, 100, 1_000)), 1_000);
            let idle = meter::FightConfig::default().idle_timeout_ms;
            let state = pipeline.tick(1_000 + idle);
            (state, 1_000 + idle)
        }

        /// A `RetentionPolicy` with no duration floor: `ended_snapshot`'s
        /// scripted single-hit fight is only a handful of milliseconds long,
        /// which the default policy's 5s floor would otherwise reject at
        /// `HistoryStore::insert` — a `RetentionPolicy` concern this test
        /// suite isn't exercising.
        fn no_floor_policy() -> history::RetentionPolicy {
            history::RetentionPolicy {
                min_duration_ms: 0,
                ..history::RetentionPolicy::default()
            }
        }

        /// Lists the history back, newest first.
        fn list_rows(handle: &HistoryHandle) -> Vec<history::EncounterSummary> {
            let (reply_tx, reply_rx) = crossbeam_channel::unbounded();
            handle.list(10, &reply_tx);
            match reply_rx.recv().unwrap() {
                HistoryEvent::Listed(rows) => rows,
                other => panic!("expected Listed, got {other:?}"),
            }
        }

        /// Lists the history back and returns how many rows it holds.
        fn row_count(handle: &HistoryHandle) -> usize {
            list_rows(handle).len()
        }

        /// A player hit on monster `target_uid`, optionally the killing
        /// blow. The module-level `damage` helper is pinned to one target
        /// uid; the phase tests need two distinct boss entities.
        fn hit_on(target_uid: i64, value: i64, ts: u64, is_dead: bool) -> proto::ProtocolEvent {
            proto::ProtocolEvent::Damage(proto::DamageEvent {
                target_uid,
                is_dead,
                ..damage(1, value, ts)
            })
        }

        /// The `EnemyHp` that names `uid` as monster template `monster_id` —
        /// the only way the meter ever learns an entity is a boss, and so a
        /// prerequisite for both the boss-death end and the phase grouping.
        fn boss_appear(
            uid: i64,
            monster_id: u32,
            curr: u64,
            max: u64,
            ts: u64,
        ) -> proto::ProtocolEvent {
            proto::ProtocolEvent::EnemyHp(proto::EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
                position: None,
                target_position: None,
            })
        }

        #[test]
        fn an_ended_fight_is_recorded_once() {
            let path = temp_history_path("pipeline-record-once");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            let (state, now) = ended_snapshot(&mut pipeline);
            pipeline.record_fight_end(state, now);
            pipeline.record_fight_end(state, now);
            pipeline.record_fight_end(state, now);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(count, 1);
        }

        /// Issue #post-end-grace: `record_fight_end` must not send to
        /// history until `Meter::fight_config`'s `post_end_grace_ms` has
        /// fully elapsed past the fight's end — otherwise the saved row
        /// races the meter's own grace window (`Meter::apply_damage_grace`)
        /// and misses whatever trailing packets that window was built to
        /// keep. A small idle timeout (well under the 2s default grace) is
        /// used here so the fight can be observed `Ended` long before grace
        /// closes, which `ended_snapshot`'s stock 9s idle timeout cannot
        /// do (9s already exceeds the 2s grace by the time `tick` ever
        /// sees `Ended` at all).
        #[test]
        fn recording_waits_for_the_grace_window_to_close() {
            let path = temp_history_path("pipeline-grace-delay");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());
            pipeline.meter.set_fight_config(meter::FightConfig {
                idle_timeout_ms: 50,
                ..meter::FightConfig::default()
            });
            let grace = pipeline.meter.fight_config().post_end_grace_ms;

            pipeline.step(proto::ProtocolEvent::Damage(damage(1, 100, 1_000)), 1_000);
            let state = pipeline.tick(1_000 + 50);
            assert_eq!(state, meter::FightState::Ended);
            let ended_at = pipeline.meter.fight_end_ms().unwrap();

            // Still inside the grace window: nothing sent yet.
            pipeline.record_fight_end(state, ended_at + grace - 1);
            assert_eq!(row_count(&handle), 0, "must not race the grace window");

            // The far edge is still *inside* the window: the meter's
            // `in_post_end_grace_window` is inclusive there, so a packet
            // stamped exactly here is still folded in and the record must
            // not go out yet (PR #333 review, finding 4).
            pipeline.record_fight_end(state, ended_at + grace);
            assert_eq!(row_count(&handle), 0, "the far edge is inclusive");

            // One millisecond later the window really is closed: the
            // deferred record goes out.
            pipeline.record_fight_end(state, ended_at + grace + 1);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(count, 1);
        }

        /// Issues #124/#316 x #post-end-grace: a phase-1 boss dying, one
        /// `Ended` tick observed inside the grace window, and then the
        /// phase-2 boss's first hit resuming the *same* fight, must leave
        /// exactly one history row covering both phases — not the pending
        /// phase-1-only row plus a complete one (PR #333 review, finding 2).
        #[test]
        fn a_phase_resume_inside_the_grace_window_records_one_row() {
            /// Dragonbane Golem - Right Cannon and - Left Cannon: two
            /// phases of one curated fight
            /// (`bpsr_meter::phase::BOSS_PHASE_GROUPS`).
            const RIGHT_CANNON: u32 = 103_110;
            const LEFT_CANNON: u32 = 103_111;

            let path = temp_history_path("pipeline-phase-resume-grace");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());
            let grace = pipeline.meter.fight_config().post_end_grace_ms;

            // Phase 1: 100 damage, then the cannon dies at 2_000.
            pipeline.step(boss_appear(10, RIGHT_CANNON, 900, 1_000, 1_000), 1_000);
            pipeline.step(hit_on(10, 100, 1_000, false), 1_000);
            pipeline.step(hit_on(10, 100, 2_000, true), 2_000);
            let state = pipeline.tick(2_100);
            assert_eq!(state, meter::FightState::Ended, "the kill ends the fight");
            assert_eq!(pipeline.meter.fight_end_ms(), Some(2_000));

            // One `Ended` tick well inside the grace window: the record is
            // captured but nothing goes out.
            pipeline.record_fight_end(state, 2_100);
            assert_eq!(row_count(&handle), 0, "must not race the grace window");
            assert!(pipeline.pending_fight_end.is_some(), "captured, not sent");

            // Phase 2 spawns and takes its first hit, still inside the
            // grace window: the held fight resumes rather than restarting.
            pipeline.step(boss_appear(11, LEFT_CANNON, 500, 500, 2_500), 2_500);
            pipeline.step(hit_on(11, 300, 2_500, false), 2_500);
            let state = pipeline.tick(2_600);
            assert_eq!(state, meter::FightState::Active, "the same fight resumed");
            assert_eq!(
                pipeline.meter.fight_start_ms(),
                Some(1_000),
                "a resume keeps the fight clock, which is how the pipeline knows"
            );

            pipeline.record_fight_end(state, 2_600);
            assert_eq!(
                row_count(&handle),
                0,
                "a resumed fight must not leave a truncated phase-1 row"
            );
            assert!(
                pipeline.pending_fight_end.is_none(),
                "discarded, not queued"
            );

            // Phase 2 dies for real, and the window closes on that end.
            pipeline.step(hit_on(11, 100, 3_000, true), 3_000);
            let state = pipeline.tick(3_100);
            assert_eq!(state, meter::FightState::Ended);
            pipeline.record_fight_end(state, 3_000 + grace + 1);

            let rows = list_rows(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(rows.len(), 1, "one fight, one row");
            assert_eq!(
                rows[0].total_damage, 600,
                "the row must carry both phases' damage"
            );
        }

        /// With no history handle attached there is nothing to write, so
        /// `record_fight_end` returns before touching the meter at all —
        /// no `Meter::snapshot` ~10 times a second for the whole grace
        /// window, and nothing queued for a flush that could never send
        /// (PR #333 review, finding 1).
        #[test]
        fn no_history_handle_skips_the_record_entirely() {
            let mut pipeline = Pipeline::new();

            let (state, now) = ended_snapshot(&mut pipeline);
            pipeline.record_fight_end(state, now);

            assert!(pipeline.pending_fight_end.is_none(), "nothing built");
            assert!(pipeline.held_fight_start_ms.is_none(), "nothing captured");
            assert!(!pipeline.fight_end_recorded, "no latch to set");
        }

        #[test]
        fn a_new_fight_clears_the_recorded_latch() {
            let path = temp_history_path("pipeline-clear-latch");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            let (state, now) = ended_snapshot(&mut pipeline);
            pipeline.record_fight_end(state, now);
            pipeline.record_fight_end(meter::FightState::Active, now);
            pipeline.record_fight_end(state, now);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(count, 2);
        }

        /// Pipeline-robustness audit, finding 3: `record_fight_end` used to
        /// run only from `publish`'s 100ms ticker. A boss death latches
        /// `FightState::Ended` synchronously inside `Meter::apply` (unlike
        /// the idle timeout, which needs `tick`) — so a fight that ends this
        /// way and is then reset by a new pull's first hit, both inside one
        /// tick window with the ticker never firing in between, used to
        /// vanish from history entirely: `publish`'s next call only ever
        /// saw the *new* fight's `Active` state. `Pipeline::step` now calls
        /// `record_fight_end` itself, right after applying each event, so
        /// the `Ended` state is caught the instant the boss dies — with no
        /// `pipeline.tick()` call anywhere in this test.
        #[test]
        fn a_boss_death_followed_by_a_new_fight_hit_with_no_tick_between_still_records() {
            let path = temp_history_path("pipeline-event-driven-record");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            // Engage a catalogued boss — monster id 103 ("Ignisor"), the same
            // id `bpsr_meter::encounter`'s own
            // `a_recognized_boss_dying_ends_the_fight_immediately` uses.
            pipeline.step(proto::ProtocolEvent::Damage(damage(1, 100, 0)), 0);
            pipeline.step(
                proto::ProtocolEvent::EnemyHp(proto::EnemyHp {
                    uid: 500,
                    curr_hp: Some(50),
                    max_hp: Some(100),
                    monster_id: Some(103),
                    timestamp_ms: 0,
                    position: None,
                    target_position: None,
                }),
                0,
            );
            // The killing blow: `Meter::apply` latches
            // `FightEndCause::BossDeath` synchronously, no `tick` involved.
            pipeline.step(
                proto::ProtocolEvent::Damage(proto::DamageEvent {
                    is_dead: true,
                    ..damage(1, 1_000, 1_000)
                }),
                1_000,
            );

            // A new fight's first hit, 40ms later, on an unrelated add
            // (uid 999, never engaged by the fight that just ended) — still
            // inside a single 100ms publish tick, and no `pipeline.tick()`
            // call anywhere in this test. Issue #post-end-grace: a hit on
            // the *same* dead target would be folded into the grace window
            // instead of resetting (see `a_phase_resume_inside_the_grace_
            // window_records_one_row`), so this must target a different uid
            // to still exercise the no-tick reset-driven flush.
            pipeline.step(hit_on(999, 100, 1_040, false), 1_040);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(
                count, 1,
                "the boss-death-ended fight must be recorded even though no tick ever ran \
                 before the next fight's first hit reset the meter"
            );
        }

        #[test]
        fn an_idle_pipeline_records_nothing() {
            let path = temp_history_path("pipeline-idle-none");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            pipeline.record_fight_end(meter::FightState::Idle, 0);
            pipeline.record_fight_end(meter::FightState::Active, 0);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(count, 0);
        }

        /// PR #329 review, finding 1: `Meter::fight_state` reports an
        /// idle-timeout `Ended` without latching it (only `Meter::tick`
        /// calls `latch_fight_end`), so `step`'s own `record_fight_end`
        /// call can see `Ended` while `fight_end_ms` is still `None`.
        /// `record_fight_end` used to set its write-exactly-once latch
        /// *before* checking `fight_end_ms`, so one non-damage event
        /// arriving after the idle timeout but before the ticker's next
        /// `tick` poisoned the latch and the fight was lost from history
        /// entirely. The latch is now set only once `fight_end_ms` is
        /// `Some`, which makes that observation a retryable no-op.
        #[test]
        fn a_non_damage_event_after_the_idle_timeout_does_not_lose_the_fight() {
            let path = temp_history_path("pipeline-unlatched-end");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            pipeline.step(proto::ProtocolEvent::Damage(damage(1, 100, 1_000)), 1_000);
            let idle = meter::FightConfig::default().idle_timeout_ms;
            let after_idle = 1_000 + idle;

            // The fight is over by wall clock, but nothing has latched that
            // yet — no `tick` has run.
            assert!(
                pipeline.meter.fight_end_ms().is_none(),
                "sanity: the idle-timeout end must still be unlatched here"
            );
            assert_eq!(
                pipeline.meter.fight_state(after_idle),
                meter::FightState::Ended
            );

            // A non-damage event routed through `step` — the exact call
            // that used to poison the latch. A cast is the cleanest of the
            // candidates: `Encounter::apply_cast` never resets and never
            // extends the DPS window, so the only thing under test here is
            // `step`'s own `record_fight_end` call.
            pipeline.step(
                proto::ProtocolEvent::Cast(proto::event::CastEvent {
                    caster_uid: 1,
                    skill_id: 7,
                    timestamp_ms: after_idle,
                    skill_stage: None,
                    skill_level: None,
                    skill_begin_time_ms: None,
                    skill_stage_num: None,
                    skill_uuid: None,
                }),
                after_idle,
            );

            // The ticker finally runs and latches the end, exactly as
            // `publish` does.
            let state = pipeline.tick(after_idle);
            pipeline.record_fight_end(state, after_idle);

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(
                count, 1,
                "a non-damage event seen after the idle timeout but before the next tick must \
                 not stop the fight from reaching history"
            );
        }

        /// Issue #321: `run`'s `UiCommand::Quit`/disconnect break arms both
        /// call `publish` one last time before the thread exits, so a fight
        /// already sitting in `FightState::Ended` at quit time still reaches
        /// history — without it, that fight would simply never be recorded,
        /// since `record_fight_end` otherwise only runs from the 100ms tick
        /// arm and quitting drops the channel (and with it, any chance of
        /// another tick) the moment the loop breaks. This drives `publish`
        /// directly — the exact call the Quit/disconnect arms make — rather
        /// than the real `spawn`-ed thread, so the test does not have to
        /// race a live 100ms ticker to keep the periodic tick from
        /// recording the fight first and masking a regression here.
        #[test]
        fn a_final_publish_at_quit_records_an_already_ended_fight() {
            let path = temp_history_path("pipeline-quit-flush");
            let (handle, thread) = HistoryHandle::spawn(path.clone(), no_floor_policy()).unwrap();
            let mut pipeline = Pipeline::new().with_history(handle.clone());

            let (state, _now) = ended_snapshot(&mut pipeline);
            assert_eq!(
                state,
                meter::FightState::Ended,
                "sanity: the scripted fight must already be Ended before the flush"
            );

            // Mirrors `spawn`'s own channel setup — a real `stale` clone of
            // `tx_snapshot`'s receiver, exactly what `publish` needs for its
            // drop-the-stale-and-retry fallback.
            let (tx_snapshot, rx_snapshot) = bounded::<meter::Snapshot>(1);
            let stale = rx_snapshot.clone();
            let skill_focus: Vec<i64> = Vec::new();
            let repaint = RepaintHandle::new();
            let mut last_published: Option<meter::Snapshot> = None;

            // The call `Ok(UiCommand::Quit)` and `Err(_)` both make just
            // before `break`.
            publish(
                &mut pipeline,
                &tx_snapshot,
                &stale,
                &skill_focus,
                &repaint,
                &mut last_published,
            );

            let count = row_count(&handle);
            drop(handle);
            drop(pipeline);
            let _ = thread.join();
            let _ = std::fs::remove_file(&path);

            assert_eq!(
                count, 1,
                "the final publish at quit must record the already-ended fight"
            );
        }

        /// Drives `ctx` through egui passes until it stops asking for a
        /// repaint of its own accord, so a following
        /// `has_requested_repaint` reads only what the code under test
        /// did. A brand-new `egui::Context` starts out *already*
        /// requesting repaints (it needs a few warm-up passes to upload
        /// its font texture and settle), and a `request_repaint` made
        /// between passes stays visible through the pass that consumes it
        /// — so "run one pass" is not enough to clear the flag in either
        /// direction. `FullOutput::textures_delta` must be cleared before
        /// it drops or epaint panics about unapplied deltas.
        fn settle_repaints(ctx: &egui::Context) {
            for _ in 0..10 {
                if !ctx.has_requested_repaint() {
                    return;
                }
                let mut out = ctx.run_ui(Default::default(), |_| {});
                out.textures_delta.clear();
            }
            panic!("egui context never stopped requesting repaints on its own");
        }

        /// Issue #349: `publish` must wake the overlay the first time it
        /// has anything to say (there is no prior snapshot to compare
        /// against), but must not wake it again for a second, unchanged
        /// publish — the overwhelmingly common case while the game sits
        /// idle, and exactly the case a fixed-clock heartbeat used to be
        /// the only thing covering.
        #[test]
        fn publish_wakes_only_on_a_changed_snapshot() {
            let mut pipeline = Pipeline::new();
            let (tx_snapshot, rx_snapshot) = bounded::<meter::Snapshot>(1);
            let stale = rx_snapshot.clone();
            let skill_focus: Vec<i64> = Vec::new();
            let repaint = RepaintHandle::new();
            let ctx = egui::Context::default();
            repaint.install(ctx.clone());
            let mut last_published: Option<meter::Snapshot> = None;

            settle_repaints(&ctx);

            publish(
                &mut pipeline,
                &tx_snapshot,
                &stale,
                &skill_focus,
                &repaint,
                &mut last_published,
            );
            assert!(
                ctx.has_requested_repaint(),
                "the first publish has no prior snapshot to compare against, so it must wake \
                 the overlay"
            );

            settle_repaints(&ctx);

            publish(
                &mut pipeline,
                &tx_snapshot,
                &stale,
                &skill_focus,
                &repaint,
                &mut last_published,
            );
            assert!(
                !ctx.has_requested_repaint(),
                "a second publish with nothing new in the pipeline must not wake the overlay again"
            );
        }

        #[test]
        fn a_pipeline_without_history_never_panics() {
            let mut pipeline = Pipeline::new();
            pipeline.record_fight_end(meter::FightState::Ended, 0);
        }
    }

    /// PR #329 review, finding 2: telling an orderly shutdown (`main.rs`
    /// queues `UiCommand::Quit`, then stops capture, which drops
    /// `tx_events`) apart from the capture thread actually dying.
    mod orderly_shutdown {
        use super::*;

        #[test]
        fn a_queued_quit_marks_the_disconnect_as_orderly() {
            let (tx, rx) = crossbeam_channel::unbounded();
            tx.send(UiCommand::Quit).unwrap();
            let mut pipeline = Pipeline::new();
            let mut skill_focus = Vec::new();

            assert!(drain_for_quit(&rx, &mut pipeline, &mut skill_focus, &None));
            assert!(
                pipeline.capture_alive,
                "an orderly shutdown must never mark capture dead"
            );
        }

        /// The drain applies the commands it passes on the way rather than
        /// discarding them — a `SkillFocus` the overlay sent just before
        /// closing is still the pipeline's focus set afterwards.
        #[test]
        fn commands_ahead_of_the_quit_are_still_applied() {
            let (tx, rx) = crossbeam_channel::unbounded();
            tx.send(UiCommand::SkillFocus(vec![7, 9])).unwrap();
            tx.send(UiCommand::Quit).unwrap();
            let mut pipeline = Pipeline::new();
            let mut skill_focus = Vec::new();

            assert!(drain_for_quit(&rx, &mut pipeline, &mut skill_focus, &None));
            assert_eq!(skill_focus, vec![7, 9]);
        }

        /// No queued `Quit` is the genuine crash case: the caller falls
        /// through to the ERROR log and `mark_capture_dead`.
        #[test]
        fn a_disconnect_with_no_quit_queued_is_still_a_crash() {
            let (tx, rx) = crossbeam_channel::unbounded();
            tx.send(UiCommand::Reset).unwrap();
            let mut pipeline = Pipeline::new();
            let mut skill_focus = Vec::new();

            assert!(!drain_for_quit(&rx, &mut pipeline, &mut skill_focus, &None));
        }

        /// End to end through the real spawned thread, in `main.rs`'s
        /// shutdown order: `Quit` queued first, capture's sender dropped
        /// second. The thread must exit without ever publishing a
        /// `capture_alive: false` snapshot.
        #[test]
        fn quit_before_the_capture_sender_drops_never_reports_a_dead_capture() {
            use bpsr_test_support::scratch_path;

            let (tx_events, rx_events) = crossbeam_channel::unbounded();
            let (tx_command, rx_command) = crossbeam_channel::unbounded();
            let (rx_snapshot, thread) = spawn(
                rx_events,
                rx_command,
                scratch_path("orderly-shutdown"),
                None,
                None,
                RepaintHandle::new(),
            );

            tx_command.send(UiCommand::Quit).unwrap();
            drop(tx_events);
            thread.join().unwrap();

            while let Ok(snap) = rx_snapshot.try_recv() {
                assert!(
                    snap.capture_alive,
                    "an orderly shutdown must not publish capture_alive = false"
                );
            }
        }
    }

    /// Issue #214: the header dropdown's "Restart packet capture" item
    /// sends `UiCommand::RestartCapture` down the same channel Reset and
    /// Quit already use, and this thread is what turns it into a request
    /// the capture thread will read. Driven through the real spawned
    /// pipeline, since the routing *is* the behaviour under test.
    #[test]
    fn a_restart_capture_command_reaches_the_capture_thread() {
        use bpsr_test_support::scratch_path;
        use std::time::{Duration, Instant};

        let (_tx_events, rx_events) = crossbeam_channel::unbounded();
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let restart = bpsr_capture::CaptureRestart::new();

        // Held for the pipeline's whole life: its snapshot channel is
        // `bounded(1)` with a drop-the-stale fallback, so a dropped
        // receiver would be a disconnect rather than back-pressure.
        let (_rx_snapshot, thread) = spawn(
            rx_events,
            rx_command,
            scratch_path("restart-capture"),
            None,
            Some(restart.clone()),
            RepaintHandle::new(),
        );

        tx_command.send(UiCommand::RestartCapture).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requested = false;
        while Instant::now() < deadline {
            if restart.take_requested() {
                requested = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        tx_command.send(UiCommand::Quit).unwrap();
        thread.join().unwrap();

        assert!(
            requested,
            "RestartCapture must reach the capture thread's restart flag"
        );
    }

    /// The same command with no capture running (`start_capture` failed, so
    /// `main.rs` has no handle to take a requester from) must be a no-op
    /// the pipeline survives, not a panic or an exit.
    #[test]
    fn a_restart_capture_command_without_capture_is_harmless() {
        use bpsr_test_support::scratch_path;

        let (_tx_events, rx_events) = crossbeam_channel::unbounded();
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (_rx_snapshot, thread) = spawn(
            rx_events,
            rx_command,
            scratch_path("restart-capture-none"),
            None,
            None,
            RepaintHandle::new(),
        );

        tx_command.send(UiCommand::RestartCapture).unwrap();
        // Still alive and still listening: `Quit` is what stops it.
        tx_command.send(UiCommand::Quit).unwrap();
        thread.join().unwrap();
    }

    /// Pipeline-robustness audit, finding 1: dropping `tx_events` (what a
    /// panicked or exited capture thread does) used to leave the overlay
    /// with no signal at all — the snapshot channel this test reads from
    /// never disconnects, since the pipeline thread stays alive and keeps
    /// publishing on schedule. `run`'s events-`Err` arm now calls
    /// `Pipeline::mark_capture_dead`, which `snapshot_focused` stamps onto
    /// every snapshot published from then on. Driven through the real
    /// spawned pipeline, since the wiring from "channel closes" to "flag on
    /// the published snapshot" *is* the behaviour under test.
    #[test]
    fn a_dead_capture_channel_surfaces_as_capture_alive_false_on_every_later_snapshot() {
        use bpsr_test_support::scratch_path;
        use std::time::{Duration, Instant};

        let (tx_events, rx_events) = crossbeam_channel::unbounded();
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (rx_snapshot, thread) = spawn(
            rx_events,
            rx_command,
            scratch_path("capture-dead-status"),
            None,
            None,
            RepaintHandle::new(),
        );

        // The capture thread panicking or exiting drops its `Sender` half;
        // nothing else in this process holds one.
        drop(tx_events);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_dead = false;
        while Instant::now() < deadline {
            if let Ok(snap) = rx_snapshot.recv_timeout(Duration::from_millis(50))
                && !snap.capture_alive
            {
                saw_dead = true;
                break;
            }
        }

        tx_command.send(UiCommand::Quit).unwrap();
        thread.join().unwrap();

        assert!(
            saw_dead,
            "a dropped capture-event sender must publish capture_alive = false"
        );
    }
}
