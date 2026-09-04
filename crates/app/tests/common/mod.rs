//! System-test harness (`docs/plans/system-test-harness.md` §3): drives real
//! wire bytes (and the one event the decoder can never produce,
//! `ServerChanged`) through `TcpReassembler` -> `Decoder::push_stream` ->
//! `Pipeline::step`/`tick`/`snapshot`, and compares the resulting
//! `Snapshot`s against checked-in text goldens.
//!
//! Each test binary that `mod common;`s this module only uses part of the
//! API below, and CI runs with `-D warnings`, so the whole module is
//! `#[allow(dead_code)]`.
//!
//! To regenerate goldens after an intentional behavior change:
//! ```text
//! SHINRA_UPDATE_GOLDENS=1 cargo test -p ShinraMeter-BPSR --tests
//! git diff crates/app/tests/goldens
//! ```
//! then read every changed line and confirm it is what the scenario intends
//! before re-running **without** the variable.
#![allow(dead_code)]

use std::cmp::Reverse;
use std::path::Path;

use bpsr_app::history::writer::HistoryHandle;
use bpsr_app::history::{EncounterRecord, EncounterSummary, RetentionPolicy};
use bpsr_app::pipeline::Pipeline;
use bpsr_capture::tcp::TcpReassembler;
use bpsr_meter::{FightState, PlayerRow, ResetReason, Snapshot};
use bpsr_protocol::Decoder;
use bpsr_test_support::scenario::{Delivery, Scenario, Step};

/// A fixed, arbitrary TCP sequence baseline. Any value works; a mid-range one
/// keeps the scenarios away from the u32 wrap edge.
const INITIAL_SEQ: u32 = 0x1000_0000;

/// Drives a [`Scenario`] through the real capture/protocol/pipeline stack.
pub struct Rig {
    reassembler: TcpReassembler,
    decoder: Decoder,
    pipeline: Pipeline,
    seq: u32,
    /// The entity table `feed_notify` decodes against (issue #335). The
    /// `Decoder` above owns its own for the byte-stream path; this is the
    /// same cross-packet state for the path that hands `Notify`s in
    /// directly.
    entities: bpsr_protocol::EntityTable,
    resets: Vec<(u64, ResetReason)>,
    fight_state: FightState,
    /// The history thread's handle (issue #39), if `with_history` attached
    /// one. `None` for every existing test, which is what keeps this change
    /// inert for them — `Pipeline::record_fight_end` is a no-op without a
    /// `history` handle.
    history: Option<HistoryHandle>,
}

/// One `Step::Capture` observed while running a [`Scenario`].
pub struct Capture {
    pub label: &'static str,
    pub at_ms: u64,
    pub snapshot: Snapshot,
    pub fight_state: FightState,
    /// Every reset observed since the scenario started, in order.
    pub resets: Vec<(u64, ResetReason)>,
}

impl Rig {
    pub fn new() -> Self {
        let mut reassembler = TcpReassembler::new();
        // Anchor the stream so an out-of-order first segment cannot
        // re-baseline it. `resync` also clears the loss flag, so nothing to
        // drain here.
        reassembler.resync(INITIAL_SEQ);
        Self {
            reassembler,
            decoder: Decoder::new(),
            pipeline: Pipeline::new(),
            seq: INITIAL_SEQ,
            entities: bpsr_protocol::EntityTable::new(),
            resets: Vec::new(),
            fight_state: FightState::Idle,
            history: None,
        }
    }

    /// Attaches a history thread (issue #39) so a scenario's fight ends land
    /// in a real database. `path` is the test's own temp file — `Rig` never
    /// touches `%APPDATA%`. Rebuilds `self.pipeline` fresh (this `Rig` never
    /// configures the name/scene-boss caches, so there is nothing else to
    /// preserve) so the returned `Rig` records every fight end through the
    /// real `Pipeline::step`/`tick`/`record_fight_end` edge, exactly like
    /// production.
    pub fn with_history(
        mut self,
        path: std::path::PathBuf,
        policy: RetentionPolicy,
    ) -> (Self, std::thread::JoinHandle<()>) {
        let (handle, thread) =
            HistoryHandle::spawn(path, policy).expect("open the test history store");
        self.history = Some(handle.clone());
        self.pipeline = Pipeline::new().with_history(handle);
        (self, thread)
    }

    /// The history handle attached by `with_history`, if any — for a test
    /// that wants to `list`/`load`/`delete`/`clear` directly against the
    /// same store the scenario just wrote to.
    pub fn history(&self) -> Option<&HistoryHandle> {
        self.history.as_ref()
    }

    /// Runs every step of `scenario` in order, returning the `Capture`s taken
    /// along the way (one per `Step::Capture`).
    pub fn run(&mut self, scenario: &Scenario) -> Vec<Capture> {
        let mut captures = Vec::new();
        for step in &scenario.steps {
            match step {
                Step::Bytes {
                    at_ms,
                    bytes,
                    delivery,
                } => self.run_bytes(*at_ms, bytes, delivery),
                Step::Inject { at_ms, event } => {
                    if let Some(reason) = self.pipeline.step(event.clone(), *at_ms) {
                        self.resets.push((*at_ms, reason));
                    }
                }
                Step::Tick { at_ms } => {
                    self.fight_state = self.pipeline.tick(*at_ms);
                    self.pipeline.record_fight_end(self.fight_state, *at_ms);
                }
                Step::Capture { at_ms, label } => {
                    captures.push(Capture {
                        label,
                        at_ms: *at_ms,
                        snapshot: self.pipeline.snapshot(*at_ms),
                        fight_state: self.fight_state,
                        resets: self.resets.clone(),
                    });
                }
            }
        }
        captures
    }

    /// Feeds one already-decompressed Notify body straight into
    /// `decode_notify -> Pipeline::step`, skipping `TcpReassembler`/
    /// `Decoder::push_stream` entirely. For replaying dump-format records
    /// (`bpsr_protocol::dump_format::DumpRecord`): those are already
    /// post-zstd and post-frame-split, so re-entering at the TCP-byte layer
    /// (`run`/`run_bytes`) would be the wrong seam — there is no raw stream
    /// to reassemble, just a `(service_uuid, method_id, payload)` triple per
    /// record. Does
    /// not touch `self.reassembler`/`self.decoder`, so it cannot perturb
    /// `run`'s behavior; the two entry points share only `self.pipeline`
    /// and `self.resets`.
    pub fn feed_notify(&mut self, service_uuid: u64, method_id: u32, payload: &[u8], ts_ms: u64) {
        let notify = bpsr_protocol::frame::Notify {
            service_uuid,
            method_id,
            payload: payload.to_vec(),
        };
        let mut events = Vec::new();
        bpsr_protocol::decode::decode_notify(
            &notify,
            ts_ms,
            &mut events,
            None,
            &mut self.entities,
        );
        for ev in events {
            if let Some(reason) = self.pipeline.step(ev, ts_ms) {
                self.resets.push((ts_ms, reason));
            }
        }
    }

    /// Advances the meter's idle-timeout state machine, mirroring what
    /// `Step::Tick` does inside `run` (issue #39: including the
    /// `record_fight_end` edge trigger). Exposed directly for
    /// `feed_notify`-driven tests, which don't go through `Scenario`/`run`.
    pub fn tick(&mut self, now_ms: u64) -> FightState {
        self.fight_state = self.pipeline.tick(now_ms);
        self.pipeline.record_fight_end(self.fight_state, now_ms);
        self.fight_state
    }

    /// Takes a snapshot without recording a `Capture`, for a
    /// `feed_notify`-driven test that wants to build its own `Capture`
    /// (label, at_ms, resets) around it.
    pub fn snapshot(&self, now_ms: u64) -> Snapshot {
        self.pipeline.snapshot(now_ms)
    }

    /// Every reset observed since the scenario started, in order — the same
    /// data a `Capture` carries, for a `feed_notify`-driven test to build one
    /// with directly.
    pub fn resets(&self) -> Vec<(u64, ResetReason)> {
        self.resets.clone()
    }

    /// The fight state as of the last `tick` (or `Idle` if `tick` was never
    /// called).
    pub fn fight_state(&self) -> FightState {
        self.fight_state
    }

    fn run_bytes(&mut self, at_ms: u64, bytes: &[u8], delivery: &Delivery) {
        // 1. Cut `bytes` into pieces, in stream order.
        let pieces = cut(bytes, delivery);
        // 2. Assign each piece its stream-order seq: base, base+len0, ...
        let mut seqs = Vec::with_capacity(pieces.len());
        let mut seq = self.seq;
        for piece in &pieces {
            seqs.push(seq);
            seq += piece.len() as u32;
        }
        // 3. Advance self.seq by bytes.len() (== the sum of piece lengths).
        self.seq = seq;
        // 4. Push pieces in delivery order (sequence numbers stay in stream
        //    order regardless of the order they're pushed in).
        let push_order: Vec<usize> = match delivery {
            Delivery::Whole | Delivery::SplitAt(_) => (0..pieces.len()).collect(),
            Delivery::SplitAndReorder { order, .. } => order.clone(),
        };
        for idx in push_order {
            self.reassembler.push(seqs[idx], &pieces[idx]);
        }
        // 5. A dropped segment invalidates whatever the decoder has
        //    buffered so far.
        if self.reassembler.take_loss() {
            self.decoder.reset();
        }
        // 6. Drain the now-contiguous stream and feed it to the decoder.
        let stream = self.reassembler.take_stream();
        for ev in self.decoder.push_stream(&stream, at_ms) {
            if let Some(reason) = self.pipeline.step(ev, at_ms) {
                self.resets.push((at_ms, reason));
            }
        }
    }
}

impl Default for Rig {
    fn default() -> Self {
        Self::new()
    }
}

/// Cuts `bytes` into pieces per `delivery`, in stream order (i.e. the order
/// the bytes appear in the original slice, not the delivery/push order).
fn cut(bytes: &[u8], delivery: &Delivery) -> Vec<Vec<u8>> {
    match delivery {
        Delivery::Whole => vec![bytes.to_vec()],
        Delivery::SplitAt(at) => split_at_offsets(bytes, at),
        Delivery::SplitAndReorder { at, .. } => split_at_offsets(bytes, at),
    }
}

fn split_at_offsets(bytes: &[u8], at: &[usize]) -> Vec<Vec<u8>> {
    let mut offsets = at.to_vec();
    offsets.sort_unstable();
    let mut pieces = Vec::with_capacity(offsets.len() + 1);
    let mut start = 0;
    for offset in offsets {
        pieces.push(bytes[start..offset].to_vec());
        start = offset;
    }
    pieces.push(bytes[start..].to_vec());
    pieces
}

// --- golden rendering ------------------------------------------------------

/// Renders `capture` as deterministic text (see the module's plan section for
/// the exact format) and compares it with `tests/goldens/<label>.txt`.
///
/// With `SHINRA_UPDATE_GOLDENS` set in the environment, writes the file
/// instead and passes. Read-only env access (`std::env::var`), read once per
/// call — parallel-safe, unlike `set_var`.
pub fn assert_golden(capture: &Capture) {
    let rendered = render(capture);
    compare_golden(capture.label, &rendered);
}

/// Renders what the history database holds for one encounter, as
/// deterministic text — the encounter-history counterpart of `render`
/// (issue #39) — and compares it with `tests/goldens/<label>.txt` via the
/// same `compare_golden` machinery `assert_golden` uses.
///
/// `encounters` is a `list()` reply (newest first); only its first row is
/// rendered as the `summary` line, alongside the full `load()`ed `record`
/// (players included). Deliberately omits `record.meter_version` — it is
/// `env!("CARGO_PKG_VERSION")` and would churn the golden on every release
/// bump; assert that separately in code instead.
pub fn assert_history_golden(
    label: &str,
    encounters: &[EncounterSummary],
    record: &EncounterRecord,
) {
    let rendered = render_history(label, encounters, record);
    compare_golden(label, &rendered);
}

/// Shared golden-file compare/update tail for `assert_golden` and
/// `assert_history_golden` (issue #39).
///
/// With `SHINRA_UPDATE_GOLDENS` set in the environment, writes the file
/// instead and passes. Read-only env access (`std::env::var`), read once per
/// call — parallel-safe, unlike `set_var`.
fn compare_golden(label: &str, rendered: &str) {
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/goldens"));
    let path = dir.join(format!("{label}.txt"));

    if std::env::var("SHINRA_UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(dir).expect("create tests/goldens");
        std::fs::write(&path, rendered).expect("write golden file");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {path}: regenerate with `SHINRA_UPDATE_GOLDENS=1 cargo test -p ShinraMeter-BPSR --tests`, \
             read the diff, then re-run without the variable",
            path = path.display(),
        )
    });

    assert!(
        expected == rendered,
        "golden mismatch for {label:?} ({path}):\n{diff}\nregenerate with `SHINRA_UPDATE_GOLDENS=1 cargo test -p ShinraMeter-BPSR --tests`, \
         read the diff, then re-run without the variable",
        path = path.display(),
        diff = line_diff(&expected, rendered),
    );
}

/// Renders an `assert_history_golden` call as the plain deterministic golden
/// text format documented on its doc comment.
fn render_history(
    label: &str,
    encounters: &[EncounterSummary],
    record: &EncounterRecord,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("history={label}\n"));
    out.push_str(&format!("count={}\n", encounters.len()));
    if let Some(summary) = encounters.first() {
        out.push_str(&format!(
            "summary title={} subtitle={} ended_at_ms={} duration_ms={} total_damage={} total_dps={} players={}\n",
            summary.title,
            opt(summary.subtitle.clone()),
            summary.ended_at_ms,
            summary.duration_ms,
            summary.total_damage,
            fmt_f64(summary.total_dps),
            summary.player_count,
        ));
    }
    out.push_str(&format!(
        "record title={} subtitle={} boss_monster_id={} is_boss={} scene_id={} ended_at_ms={} duration_ms={} total_damage={} total_dps={}\n",
        record.title,
        opt(record.subtitle.clone()),
        opt(record.boss_monster_id),
        record.is_boss,
        opt(record.scene_id),
        record.ended_at_ms,
        record.duration_ms,
        record.total_damage,
        fmt_f64(record.total_dps),
    ));
    for (slot, player) in record.players.iter().enumerate() {
        out.push_str(&format!(
            "row slot={} uid={} name={} class={} damage={} dps={} share={} crit={} lucky={} hits={} deaths={}\n",
            slot,
            player.uid,
            player.name,
            match player.class {
                Some(class) => class.name(),
                None => "-",
            },
            player.damage,
            fmt_f64(player.dps),
            fmt_f32(player.share_pct),
            fmt_f32(player.crit_pct),
            fmt_f32(player.lucky_pct),
            player.hits,
            player.deaths,
        ));
    }
    out
}

/// Renders one `Capture` as the plain deterministic golden text format.
fn render(capture: &Capture) -> String {
    let snap = &capture.snapshot;
    let mut rows: Vec<&PlayerRow> = snap.rows.iter().collect();
    // Belt and braces against `Meter::snapshot`'s `HashMap`-iteration tie
    // order: re-sort by (damage, uid) so the golden is stable regardless.
    rows.sort_by_key(|r| (Reverse(r.damage), r.uid));

    let mut out = String::new();
    out.push_str(&format!("scenario={}\n", capture.label));
    out.push_str(&format!("at_ms={}\n", capture.at_ms));
    out.push_str(&format!("fight_state={:?}\n", capture.fight_state));
    out.push_str(&format!("duration_ms={}\n", snap.duration_ms));
    out.push_str(&format!("total_damage={}\n", snap.total_damage));
    out.push_str(&format!("total_dps={}\n", fmt_f64(snap.total_dps)));
    out.push_str(&format!(
        "encounter.boss_monster_id={}\n",
        opt(snap.encounter.boss_monster_id)
    ));
    out.push_str(&format!(
        "encounter.boss_name={}\n",
        opt(snap.encounter.boss_name)
    ));
    out.push_str(&format!("encounter.is_boss={}\n", snap.encounter.is_boss));
    out.push_str(&format!(
        "encounter.scene_id={}\n",
        opt(snap.encounter.scene_id)
    ));
    out.push_str(&format!(
        "encounter.scene_name={}\n",
        opt(snap.encounter.scene_name)
    ));
    out.push_str(&format!(
        "encounter.scene_boss_name={}\n",
        opt(snap.encounter.scene_boss_name)
    ));
    out.push_str(&format!("resets={}\n", render_resets(&capture.resets)));
    out.push_str(&format!("rows={}\n", rows.len()));
    for row in rows {
        out.push_str(&render_row(row));
        out.push('\n');
    }
    out
}

fn render_resets(resets: &[(u64, ResetReason)]) -> String {
    resets
        .iter()
        .map(|(ms, reason)| format!("{reason:?}@{ms}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn render_row(row: &PlayerRow) -> String {
    format!(
        "row uid={} name={} class={} ability_score={} season_strength={} imagines=[{},{}] \
         damage={} dps={} share={} crit={} lucky={} hits={} deaths={}",
        row.uid,
        row.name,
        match row.class {
            Some(class) => class.name(),
            None => "-",
        },
        opt(row.ability_score),
        opt(row.season_strength),
        opt(row.imagines[0]),
        opt(row.imagines[1]),
        row.damage,
        fmt_f64(row.dps),
        fmt_f32(row.share_pct),
        fmt_f32(row.crit_pct),
        fmt_f32(row.lucky_pct),
        row.hits,
        row.deaths,
    )
}

/// `Option<T>` renders as `-` when `None`, else `T`'s own `Display`. Covers
/// both `Option<&'static str>` (names may contain spaces; the format is
/// line-oriented, not whitespace-split, so that's fine) and numeric options.
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "-".to_string(),
    }
}

/// `+ 0.0` normalizes `-0.0` to `0.0` before fixed-point formatting.
fn fmt_f64(v: f64) -> String {
    format!("{:.2}", v + 0.0)
}

/// `f32` percentages are formatted from the `f32` directly — do not widen to
/// `f64` first, that changes the rounding.
fn fmt_f32(v: f32) -> String {
    format!("{:.2}", v + 0.0)
}

/// Line-by-line diff with a `-`/`+` prefix on differing lines (no external
/// diff crate).
fn line_diff(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let max = expected_lines.len().max(actual_lines.len());
    let mut out = String::new();
    for i in 0..max {
        let e = expected_lines.get(i).copied();
        let a = actual_lines.get(i).copied();
        if e != a {
            if let Some(e) = e {
                out.push_str(&format!("-{e}\n"));
            }
            if let Some(a) = a {
                out.push_str(&format!("+{a}\n"));
            }
        }
    }
    out
}
