//! Raw diagnostic frame dump writer (issue #25 slice A item 4: "Raw frame
//! dump. Write post-decompression frames to a file with timestamps, so a
//! session can be replayed offline"). Slice B's offline replay/inspect
//! binary is written against the exact format documented below — do not
//! change it without updating that reader too.
//!
//! ## On-disk format
//!
//! JSON Lines (JSONL): the file is a sequence of UTF-8 lines, each
//! terminated by `\n`, appended in the order frames were observed (never
//! rewritten). Each line is one complete, independently-parseable JSON
//! object with exactly these five fields:
//!
//! ```json
//! {"ts_ms":1699999999999,"service_uuid":"0x0000000063335342","method_id":"0x0000002d","payload_hex":"0a1b2c","payload_decoded":true}
//! ```
//!
//! - `ts_ms` — `u64`, the capture-thread wall-clock timestamp (milliseconds
//!   since the Unix epoch) at the moment the fragment was observed.
//! - `service_uuid` — the Notify fragment's service uuid: a lowercase hex
//!   string, `0x`-prefixed, zero-padded to 16 digits (a `u64`). Compare
//!   against `bpsr_protocol::frame::SERVICE_UUID` formatted the same way
//!   (`format!("0x{:016x}", ...)`) to tell a recognized service from an
//!   unrecognized one.
//! - `method_id` — the fragment's method id: a lowercase hex string,
//!   `0x`-prefixed, zero-padded to 8 digits (a `u32`).
//! - `payload_hex` — the fragment's payload — lowercase hex, two characters
//!   per byte, no separators or prefix. Empty string for a zero-length
//!   payload.
//! - `payload_decoded` — `bool`, how to read `payload_hex`. `true` (the
//!   ordinary case) means it is the payload **after** zstd decompression,
//!   i.e. exactly the bytes `bpsr_protocol::decode::decode_notify` would
//!   have decoded. `false` means decompression failed and `payload_hex` is
//!   the raw, still-compressed bytes as they arrived — a reader must not
//!   feed those to the decoder.
//!
//! Every field is a JSON string except `ts_ms` and `payload_decoded`,
//! specifically so a `service_uuid`/`method_id` value is never subject to
//! `f64` precision loss the way a bare large-integer JSON number could be.
//!
//! One record is written per `bpsr_protocol::InspectSink::on_notify` call —
//! for *every* service uuid seen, not only the recognized one, and including
//! fragments whose payload would not decompress — when sanitization
//! (issue #346, `settings::Settings::dump_sanitize`) is off. That made an
//! unsanitized dump sufficient for slice B to rebuild service/method/attr
//! id histograms without a live game session; a *sanitized* dump (the
//! default) only ever contains the seven `pb.rs`-modeled opcodes on the
//! recognized service, each re-encoded through that partial schema, so it
//! is safe to share but no longer complete enough for histogram rebuilding
//! — use `dump_sanitize: false` for that.
//!
//! Blocking file IO happens entirely on a dedicated writer thread fed over a
//! channel (mirrors `crate::settings::spawn_writer`'s dedicated-writer-thread
//! shape, adapted here to write every record instead of coalescing to the
//! latest one — a dump needs every frame, not just the newest), so it never
//! sits on the capture/decode hot path. The channel is bounded and
//! drop-on-full (see [`CAPACITY`]), so a dump is complete unless the
//! shutdown log says records were dropped.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded};
use serde::Serialize;

use crate::logging::should_rotate;
use bpsr_protocol::dump_format::{numbered_sibling, ring_siblings};

/// A dump chunk (the live file, or any numbered ring chunk) at or above this
/// size gets rotated: the live file is renamed to `<path>.1` — any existing
/// numbered chunks shift up by one first (`.1` -> `.2`, `.2` -> `.3`, ...,
/// see [`shift_ring`]) — and `path` is reopened empty. Checked both at
/// startup (a pre-existing oversized file from a prior run) and
/// continuously while the writer thread runs, mirroring `logging::Tee`.
///
/// Ten times `logging::MAX_LOG_BYTES`, deliberately — unchanged by issue
/// #322. The two files are capped for different reasons: a log grows while
/// the app merely runs, whereas a dump only grows while packet-inspection
/// diagnostics are on (`crate::inspect::enabled` — on by default since
/// issue #346, opt-out via `SHINRA_INSPECT=0`). What #322 changed is what
/// happens once a
/// *chunk* fills up: issue #285's raid emitted roughly 2.5 MB/min, so a
/// 90+ minute raid is on the order of 240 MB, and the old scheme (this
/// threshold plus exactly one retained backup, `MAX_DUMP_BYTES` from before
/// this issue) capped the whole *session* at 2x this value — losing the
/// front ~70% of a long raid with no log signal that anything had been
/// discarded. [`DEFAULT_MAX_TOTAL_RING_BYTES`] is the session-wide budget
/// now; this constant is only the per-chunk threshold within it.
const MAX_CHUNK_BYTES: u64 = 50 * 1024 * 1024;

/// Default total byte budget across every ring chunk (the live file plus
/// every numbered `.1`, `.2`, ... chunk on disk) — see [`enforce_ring_budget`].
/// [`MAX_TOTAL_BYTES_VAR`] overrides it. 512 MiB is roughly ten times the
/// old 2-chunk / 100 MiB ceiling this replaces (issue #322): at the #285
/// raid's measured ~2.5 MB/min, 512 MiB holds a bit over 3 hours — well
/// past one raid — while keeping disk use bounded rather than unbounded.
///
/// This is also the budget [`sweep_prior_sessions`] enforces against every
/// *other* session's dump files combined (issue #346) — otherwise a
/// machine that's run many past sessions accumulates their dumps forever
/// on top of the current session's own ring. Prior-session files older
/// than seven days are swept unconditionally; anything newer is kept
/// oldest-evicted-first until the survivors fit this budget.
const DEFAULT_MAX_TOTAL_RING_BYTES: u64 = 512 * 1024 * 1024;

/// Overrides [`DEFAULT_MAX_TOTAL_RING_BYTES`] when set to a positive
/// integer — an operator who knows their session runs unusually long (or
/// wants to spend less disk) can size the ring from real numbers instead of
/// the built-in guess.
const MAX_TOTAL_BYTES_VAR: &str = "SHINRA_INSPECT_MAX_BYTES";

/// [`DEFAULT_MAX_TOTAL_RING_BYTES`], or [`MAX_TOTAL_BYTES_VAR`]'s value if
/// it parses as a positive integer. `pub(crate)` (rather than private) so
/// the session-bundle export (`crate::bundle`) can report the same budget a
/// live session was actually running under, rather than re-guessing the
/// default.
pub(crate) fn max_total_ring_bytes() -> u64 {
    max_total_ring_bytes_from(std::env::var(MAX_TOTAL_BYTES_VAR).ok().as_deref())
}

fn max_total_ring_bytes_from(var: Option<&str>) -> u64 {
    var.and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_TOTAL_RING_BYTES)
}

/// Extracts the `<pid>-<secs>` session segment from a dump file name
/// (`...dump-<digits>-<digits>.jsonl`, optionally followed by a ring
/// chunk's `.<digits>` suffix — see [`bpsr_protocol::dump_format::numbered_sibling`]).
/// `None` for anything that doesn't match, which [`sweep_prior_sessions`]
/// treats as "not a dump file, leave it alone" rather than guessing.
fn dump_session_segment(name: &str) -> Option<&str> {
    let idx = name.find("dump-")?;
    let after = &name[idx + "dump-".len()..];
    let jsonl_pos = after.find(".jsonl")?;
    let session = &after[..jsonl_pos];
    let ring_suffix = &after[jsonl_pos + ".jsonl".len()..];
    if !ring_suffix.is_empty() {
        let digits = ring_suffix.strip_prefix('.')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let (pid, secs) = session.split_once('-')?;
    let valid = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (valid(pid) && valid(secs)).then_some(session)
}

/// Deletes prior sessions' inspect dump files (the live file and every
/// numbered ring chunk from a run other than `current`'s) that have either
/// aged past `max_age` or that push the *other* sessions' combined size
/// over `budget_bytes` — issue #346's dump directory otherwise accumulates
/// every past session's dump forever, on top of the current session's own
/// ring budget ([`DEFAULT_MAX_TOTAL_RING_BYTES`]).
///
/// Age-based eviction runs first (anything older than `max_age` is removed
/// outright, regardless of the budget), then the remaining prior-session
/// files are deleted oldest-mtime-first until what's left fits in
/// `budget_bytes`. `current`'s own chunks, and any file whose name doesn't
/// match the `dump-<pid>-<secs>[.n].jsonl` shape, are never touched. Best
/// effort throughout: a single file's metadata read or delete failing only
/// warns and moves on, never aborts the sweep. One `info` line summarizes
/// how many files/bytes were removed, only when at least one was.
pub(crate) fn sweep_prior_sessions(
    current: &Path,
    budget_bytes: u64,
    max_age: std::time::Duration,
) {
    let Some(parent) = current.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    let Some(current_name) = current.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Some(current_session) = dump_session_segment(current_name) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };

    let now = std::time::SystemTime::now();
    let mut candidates: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut removed_files: u64 = 0;
    let mut removed_bytes: u64 = 0;

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ty| ty.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(session) = dump_session_segment(name) else {
            continue;
        };
        if session == current_session {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let len = meta.len();
        let modified = meta.modified().unwrap_or(now);
        let age = now
            .duration_since(modified)
            .unwrap_or(std::time::Duration::ZERO);
        if age > max_age {
            match fs::remove_file(&path) {
                Ok(()) => {
                    removed_files += 1;
                    removed_bytes += len;
                }
                Err(err) => log::warn!(
                    "failed to delete stale prior-session inspect dump {}: {err}",
                    path.display()
                ),
            }
            continue;
        }
        candidates.push((path, len, modified));
    }

    candidates.sort_by_key(|(_, _, modified)| *modified);
    let mut total: u64 = candidates.iter().map(|(_, len, _)| *len).sum();
    let mut idx = 0;
    while total > budget_bytes && idx < candidates.len() {
        let (path, len, _) = &candidates[idx];
        match fs::remove_file(path) {
            Ok(()) => {
                total = total.saturating_sub(*len);
                removed_files += 1;
                removed_bytes += *len;
                idx += 1;
            }
            Err(err) => {
                log::warn!(
                    "failed to delete prior-session inspect dump {} over budget: {err}",
                    path.display()
                );
                break;
            }
        }
    }

    if removed_files > 0 {
        log::info!(
            "swept {removed_files} prior-session inspect dump file(s) totaling {removed_bytes} bytes"
        );
    }
}

/// How many records may queue up ahead of the writer thread. Bounded because
/// an unbounded channel turns a stalled writer (slow disk, AV scan, a full
/// volume) into unbounded memory growth behind a capture that never stops
/// producing. Sized like `main`'s `EVENT_CAPACITY`: deep enough to ride out a
/// burst of frames or a slow `write` without dropping anything, shallow
/// enough that the backlog stays bounded memory rather than a leak. A full
/// channel drops the record and counts it (see [`RecordSender::send`]) — the
/// dump is diagnostic output, never worth back-pressuring the decode hot
/// path for.
const CAPACITY: usize = 4096;

/// One dumped Notify-shaped fragment, in memory. See the module doc comment
/// for the exact on-disk JSON shape this serializes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub ts_ms: u64,
    pub service_uuid: u64,
    pub method_id: u32,
    pub payload: Vec<u8>,
    pub payload_decoded: bool,
}

/// The on-disk shape of one `Record` — see the module doc comment.
#[derive(Serialize)]
struct Line {
    ts_ms: u64,
    service_uuid: String,
    method_id: String,
    payload_hex: String,
    payload_decoded: bool,
}

impl From<&Record> for Line {
    fn from(r: &Record) -> Self {
        Self {
            ts_ms: r.ts_ms,
            service_uuid: format!("0x{:016x}", r.service_uuid),
            method_id: format!("0x{:08x}", r.method_id),
            payload_hex: r.payload.iter().map(|b| format!("{b:02x}")).collect(),
            payload_decoded: r.payload_decoded,
        }
    }
}

/// A cloneable handle for feeding records to the writer thread. `send` never
/// blocks and never panics: it is called from the capture/decode hot path,
/// where waiting on a stalled disk would stall packet capture itself.
#[derive(Clone)]
pub struct RecordSender {
    tx: Sender<Record>,
    dropped: Arc<AtomicU64>,
    sanitized_out: Arc<AtomicU64>,
}

impl RecordSender {
    /// Wraps a raw channel sender (the writer thread's, or a test's).
    pub fn new(tx: Sender<Record>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            sanitized_out: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Queues `record` for the writer thread, or drops it and bumps the
    /// dropped-record count if the channel is full (the writer has fallen
    /// behind) or already closed. `DumpWriter::shutdown` reports the count so
    /// an operator knows the dump on disk is incomplete.
    pub fn send(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The drop count so far, safe to read from any clone at any time —
    /// unlike `DumpWriter::shutdown`'s final tally, this doesn't need the
    /// writer thread to have exited. Used by the session-bundle export
    /// (`crate::inspect::dropped_count`) to report how incomplete an
    /// in-progress dump might be without joining the writer thread first.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many records the sanitizer (issue #346) has rejected and left
    /// out of the dump so far, safe to read from any clone at any time —
    /// same shape as [`dropped_count`](Self::dropped_count), but for
    /// records that were never queue-dropped, just judged unsafe to write.
    /// Used by `crate::inspect::sanitized_out_count`.
    pub fn sanitized_out_count(&self) -> u64 {
        self.sanitized_out.load(Ordering::Relaxed)
    }
}

/// Sending half plus the writer-thread join handle. `spawn` opens (creating
/// missing parent directories) and owns the dump file for the life of the
/// thread; every `Record` sent over the channel is appended as one line.
pub struct DumpWriter {
    tx: RecordSender,
    handle: Option<JoinHandle<()>>,
}

impl DumpWriter {
    /// Spawns the dedicated writer thread with sanitization off — every
    /// record is written to disk exactly as received. Kept for callers that
    /// need the raw stream (and for tests exercising the writer/rotation
    /// mechanics against records that aren't real, `pb.rs`-modeled
    /// protobuf); production startup uses [`spawn_sanitized`](Self::spawn_sanitized)
    /// or not, based on `settings::Settings::dump_sanitize` (issue #346).
    pub fn spawn(path: PathBuf) -> Self {
        Self::spawn_with_max_bytes(path, MAX_CHUNK_BYTES, max_total_ring_bytes(), false)
    }

    /// Spawns the dedicated writer thread with sanitization on: every
    /// record is run through `bpsr_protocol::sanitize::Sanitizer` before
    /// being written, and dropped instead of written if the sanitizer can't
    /// verify it's free of identifying data (see
    /// [`Sanitizer::sanitize_record`](bpsr_protocol::sanitize::Sanitizer::sanitize_record)).
    /// One `Sanitizer` lives for the whole writer-thread lifetime so the
    /// same player gets the same pseudonym in every record of this dump
    /// (issue #346).
    pub fn spawn_sanitized(path: PathBuf) -> Self {
        Self::spawn_with_max_bytes(path, MAX_CHUNK_BYTES, max_total_ring_bytes(), true)
    }

    /// Like [`spawn`](Self::spawn), but with both rotation thresholds
    /// overridable — only so tests can cross them without writing megabytes
    /// of records.
    fn spawn_with_max_bytes(
        path: PathBuf,
        chunk_max_bytes: u64,
        total_max_bytes: u64,
        sanitize: bool,
    ) -> Self {
        let (tx, rx) = bounded::<Record>(CAPACITY);
        let tx = RecordSender::new(tx);
        let sanitized_out = Arc::clone(&tx.sanitized_out);
        let handle = std::thread::Builder::new()
            .name("inspect-dump-writer".to_string())
            .spawn(move || {
                run_writer(
                    rx,
                    &path,
                    chunk_max_bytes,
                    total_max_bytes,
                    sanitize,
                    sanitized_out,
                )
            })
            .expect("failed to spawn the inspect-dump-writer thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// A cloneable sender for feeding records to the writer thread.
    pub fn sender(&self) -> RecordSender {
        self.tx.clone()
    }

    /// Drops this writer's own sender and blocks until the writer thread has
    /// drained (and written) every record already in flight, then logs how
    /// many records were dropped because the writer fell behind — a nonzero
    /// count means the dump on disk is incomplete, which changes how its
    /// histograms should be read. Note: any other clone of `sender()` still
    /// alive elsewhere (e.g. held by an `InspectSink` implementation) must be
    /// dropped too before the writer thread can actually exit — the channel
    /// only closes once every sender is gone.
    pub fn shutdown(mut self) {
        let handle = self.handle.take();
        // The counter outlives `self` so the tally is read *after* the join,
        // catching anything a still-live sender clone dropped on the way out.
        let dropped = Arc::clone(&self.tx.dropped);
        let sanitized_out = Arc::clone(&self.tx.sanitized_out);
        drop(self);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        let dropped = dropped.load(Ordering::Relaxed);
        let sanitized_out = sanitized_out.load(Ordering::Relaxed);
        if dropped > 0 || sanitized_out > 0 {
            log::warn!(
                "packet-inspect summary: inspect dump is INCOMPLETE — dropped {dropped} record(s) (writer thread could not keep up), sanitized-out {sanitized_out} record(s) (sanitizer rejected them)"
            );
        }
    }
}

/// Opens (or creates) the dump file for appending and writes each record it
/// receives as one JSONL line, flushing on exit. Never panics: if the file
/// can't be opened, drains the channel forever instead so senders never
/// block on a dead writer.
///
/// Also owns rotation: `written` is seeded from the (post-startup-rotation)
/// file's length and grows with every line, and the file is rotated the
/// moment the running total reaches `chunk_max_bytes` — mirrors
/// `logging::Tee`'s runtime rotation of the log file, except a rotated-out
/// chunk joins the numbered ring (see [`rotate`]) instead of overwriting a
/// single backup. Issue #322: a routine rotation used to leave no trace in
/// the log at all — `first_ts_ms`/`last_ts_ms` track the chunk currently
/// being written (reset on every *successful* rotation — see
/// [`next_first_ts_ms`] — never by re-reading the file) so
/// the info line logged right after a successful rotation says exactly what
/// was rotated out and when it happened, in-session.
///
/// When `sanitize` is set (issue #346), every record is run through a
/// session-lifetime `bpsr_protocol::sanitize::Sanitizer` before it's
/// written — see [`sanitize_record`] — and dropped instead of written if
/// the sanitizer can't verify it's free of identifying data.
fn run_writer(
    rx: Receiver<Record>,
    path: &Path,
    chunk_max_bytes: u64,
    total_max_bytes: u64,
    sanitize: bool,
    sanitized_out: Arc<AtomicU64>,
) {
    let file = match open(path, chunk_max_bytes, total_max_bytes) {
        Some(f) => f,
        None => {
            while rx.recv().is_ok() {}
            return;
        }
    };
    log::info!(
        "inspect dump writer started: path={}, chunk size={chunk_max_bytes} bytes, ring budget={total_max_bytes} bytes total (oldest chunk deleted once the ring exceeds this; override with {MAX_TOTAL_BYTES_VAR}), sanitize={sanitize}",
        path.display()
    );
    let mut written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut out = BufWriter::new(file);
    let mut first_ts_ms: Option<u64> = None;
    let mut sanitizer = sanitize.then(bpsr_protocol::sanitize::Sanitizer::new);
    while let Ok(record) = rx.recv() {
        let record = match sanitizer.as_mut() {
            Some(sanitizer) => match sanitize_record(sanitizer, record) {
                Some(record) => record,
                None => {
                    sanitized_out.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            },
            None => record,
        };
        let line = Line::from(&record);
        let json = match serde_json::to_string(&line) {
            Ok(json) => json,
            Err(err) => {
                log::warn!("inspect dump serialize failed: {err}");
                continue;
            }
        };
        if let Err(err) = writeln!(out, "{json}") {
            log::warn!("inspect dump write failed: {err}");
            continue;
        }
        if first_ts_ms.is_none() {
            first_ts_ms = Some(record.ts_ms);
        }
        // `record` is always the last one written into the chunk by the
        // time a rotation check below can fire (the write above already
        // happened this same iteration), so it doubles as the chunk's
        // last-ts_ms without a separately tracked running variable.
        let last_ts_ms = record.ts_ms;
        written += json.len() as u64 + 1; // +1 for the trailing newline
        if should_rotate(written, chunk_max_bytes) {
            let _ = out.flush();
            let bytes_rotated = written;
            let rotated = rotate(path, total_max_bytes);
            if rotated.is_some() {
                log::info!(
                    "inspect dump rotated: {bytes_rotated} bytes written, ts_ms range [{}, {last_ts_ms}]",
                    first_ts_ms.unwrap_or(0),
                );
            }
            first_ts_ms = next_first_ts_ms(rotated.is_some(), first_ts_ms);
            (out, written) = rotation_outcome(out, rotated);
        }
    }
    let _ = out.flush();
}

/// Runs `record` through `sanitizer`, converting to/from
/// `bpsr_protocol::dump_format::DumpRecord` (field-for-field identical to
/// `Record`, but owned by the protocol crate so the library-side
/// `Sanitizer` doesn't need to depend back on this crate) — `None` means
/// drop the record entirely rather than write it (see
/// `Sanitizer::sanitize_record`'s doc comment for when that happens).
fn sanitize_record(
    sanitizer: &mut bpsr_protocol::sanitize::Sanitizer,
    record: Record,
) -> Option<Record> {
    let clean = sanitizer.sanitize_record(&bpsr_protocol::dump_format::DumpRecord {
        ts_ms: record.ts_ms,
        service_uuid: record.service_uuid,
        method_id: record.method_id,
        payload: record.payload,
        payload_decoded: record.payload_decoded,
    })?;
    Some(Record {
        ts_ms: clean.ts_ms,
        service_uuid: clean.service_uuid,
        method_id: clean.method_id,
        payload: clean.payload,
        payload_decoded: clean.payload_decoded,
    })
}

/// Applies one rotation attempt's outcome to the writer's live file and byte
/// count. `Some(file)` (success) swaps in the freshly-rotated file with a
/// zeroed count. `None` (failure, already logged by [`rotate`]) keeps
/// writing to the same `current` file but *also* zeroes the count — mirrors
/// `logging::Tee::rotate`'s unconditional `self.written = 0` — so a
/// persistent rotation failure (locked `.1`, permission denial, a
/// read-only/full volume) is retried once per `max_bytes` of subsequent
/// writes rather than on every single record. Without this reset on
/// failure, `written` would stay pinned at or above `max_bytes` forever
/// once rotation starts failing, and every later record would retry the
/// failing rename and log a warning — a per-record log-spam storm instead
/// of the intended once-per-`max_bytes` backoff.
/// The chunk-start timestamp [`run_writer`] carries into the next record
/// after a rotation attempt, given whether that attempt actually rotated.
///
/// A success starts a genuinely new chunk, so the tracked start clears and
/// the next record seeds it. A failure does not: the writer keeps appending
/// to the very same file, so the chunk still starts where it always did.
/// Clearing on failure too (the original shape of this code) would make the
/// *next* successful rotation log a `ts_ms` range starting at the first
/// record written after the failure, understating the chunk's real span by
/// everything written before it — a pure function so that reasoning is
/// testable without provoking a real rename failure, which isn't portable.
fn next_first_ts_ms(rotated: bool, first_ts_ms: Option<u64>) -> Option<u64> {
    if rotated { None } else { first_ts_ms }
}

fn rotation_outcome(current: BufWriter<File>, rotated: Option<File>) -> (BufWriter<File>, u64) {
    match rotated {
        Some(file) => (BufWriter::new(file), 0),
        None => (current, 0),
    }
}

/// Renames every existing numbered chunk up by one — highest `n` first, so
/// none get clobbered mid-shift — vacating `<path>.1` for [`rotate`] to move
/// the live file into. Returns whether the whole shift succeeded.
///
/// The first failed rename stops the shift and returns `false`: the chunk
/// that failed to move is still sitting at its old number, so carrying on
/// would have the *next* rename (`.2` -> `.3` after `.3` -> `.4` failed)
/// silently overwrite it with newer data. Aborting instead leaves the ring
/// exactly as it was and costs only this one rotation — [`rotate`] returns
/// `None`, the live file keeps growing in place, and the attempt is retried
/// after another `chunk_max_bytes` of writes.
fn shift_ring(path: &Path) -> bool {
    for (n, chunk) in ring_siblings(path).into_iter().rev() {
        let to = numbered_sibling(path, n + 1);
        if let Err(err) = fs::rename(&chunk, &to) {
            log::warn!(
                "failed to shift inspect dump ring chunk {} to {} ({err}); skipping this rotation with the ring untouched, rather than letting the next shift overwrite {}",
                chunk.display(),
                to.display(),
                chunk.display()
            );
            return false;
        }
    }
    true
}

/// Deletes the oldest (highest-numbered) ring chunks until the numbered
/// chunks' total size is at or under `total_max_bytes` (issue #322's
/// numbered-ring replacement for the old single-backup cap). The live file
/// itself doesn't count towards the budget — it's still being written, and
/// is capped independently by [`MAX_CHUNK_BYTES`] triggering the next
/// rotation. Each deletion is logged at info with the chunk's path and
/// size, and nothing more: a chunk's `ts_ms` range is only ever known to
/// `run_writer` while it is the live file, and isn't carried along as later
/// rotations shift it back through the ring, so by eviction time there is
/// no range left to name.
fn enforce_ring_budget(path: &Path, total_max_bytes: u64) {
    let mut chunks: Vec<(PathBuf, u64)> = ring_siblings(path)
        .into_iter()
        .map(|(_, chunk)| {
            let len = fs::metadata(&chunk).map(|meta| meta.len()).unwrap_or(0);
            (chunk, len)
        })
        .collect();
    let mut total: u64 = chunks.iter().map(|(_, len)| len).sum();
    while total > total_max_bytes {
        let Some((oldest, len)) = chunks.pop() else {
            break;
        };
        match fs::remove_file(&oldest) {
            Ok(()) => {
                total = total.saturating_sub(len);
                log::info!(
                    "inspect dump ring exceeded its {total_max_bytes} byte budget; deleted oldest chunk {} ({len} bytes)",
                    oldest.display()
                );
            }
            Err(err) => {
                log::warn!(
                    "failed to delete oversized inspect dump ring chunk {}: {err}",
                    oldest.display()
                );
                break;
            }
        }
    }
}

/// Shifts the numbered ring up by one (see [`shift_ring`]), renames the
/// live `path` into the now-vacated `<path>.1`, enforces the total ring
/// budget (see [`enforce_ring_budget`], which may delete the oldest
/// chunk(s)), and reopens `path` empty. `None` (always already logged) when
/// the ring shift couldn't be completed — in which case the live file is
/// deliberately left alone, so nothing is renamed on top of a chunk that
/// failed to move — or when the live-file rename or the reopen fails. A
/// budget-enforcement failure only warns, since the live file has already
/// been safely rotated out by that point.
fn rotate(path: &Path, total_max_bytes: u64) -> Option<File> {
    if !shift_ring(path) {
        return None;
    }
    let first_chunk = numbered_sibling(path, 1);
    if let Err(err) = fs::rename(path, &first_chunk) {
        log::warn!(
            "failed to rotate inspect dump {} to {} ({err}); continuing without rotation",
            path.display(),
            first_chunk.display()
        );
        return None;
    }
    enforce_ring_budget(path, total_max_bytes);
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(f),
        Err(err) => {
            log::warn!(
                "failed to reopen inspect dump file {} after rotation: {err}",
                path.display()
            );
            None
        }
    }
}

/// Opens (or creates) the dump file for appending, first rotating it away if
/// it's already at or above `chunk_max_bytes` from a previous run — mirrors
/// `logging::init`'s startup rotation check.
fn open(path: &Path, chunk_max_bytes: u64, total_max_bytes: u64) -> Option<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!(
            "failed to create inspect dump directory {}: {err}",
            parent.display()
        );
        return None;
    }
    if let Some(len) = fs::metadata(path).ok().map(|meta| meta.len())
        && should_rotate(len, chunk_max_bytes)
    {
        // Reuses `rotate` for the ring-shift/budget-enforcement logic and
        // discards the fresh handle it opens — the real open happens below
        // regardless of whether this succeeds, mirroring the original
        // best-effort "continue either way" behavior.
        drop(rotate(path, total_max_bytes));
    }
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(f),
        Err(err) => {
            log::warn!("failed to open inspect dump file {}: {err}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-inspect-test-{tag}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn writer_appends_one_json_line_per_record_in_order() {
        let path = temp_path("order");
        let writer = DumpWriter::spawn(path.clone());
        writer.sender().send(Record {
            ts_ms: 1,
            service_uuid: 0xAB,
            method_id: 0x02,
            payload: vec![0xDE, 0xAD],
            payload_decoded: true,
        });
        writer.sender().send(Record {
            ts_ms: 2,
            service_uuid: 0xCD,
            method_id: 0x03,
            payload: vec![],
            payload_decoded: false,
        });
        writer.shutdown();

        let contents = fs::read_to_string(&path).expect("dump file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"{"ts_ms":1,"service_uuid":"0x00000000000000ab","method_id":"0x00000002","payload_hex":"dead","payload_decoded":true}"#
        );
        assert_eq!(
            lines[1],
            r#"{"ts_ms":2,"service_uuid":"0x00000000000000cd","method_id":"0x00000003","payload_hex":"","payload_decoded":false}"#
        );
        let _ = fs::remove_file(&path);
    }

    /// Issue #346: `spawn_sanitized` must never let a raw player name reach
    /// disk — every record is run through `bpsr_protocol::sanitize::Sanitizer`
    /// before it's written.
    #[test]
    fn spawn_sanitized_writes_no_raw_names_to_disk() {
        use prost::Message;

        let path = temp_path("sanitized");
        let payload = bpsr_protocol::pb::SyncContainerData {
            v_data: Some(bpsr_protocol::pb::CharSerialize {
                char_id: 1_646_812,
                char_base: Some(bpsr_protocol::pb::CharBaseInfo {
                    char_id: 1_646_812,
                    name: "TotallyRealPlayerName".to_string(),
                    fight_point: 12345,
                }),
                scene_data: None,
                profession_list: None,
            }),
        }
        .encode_to_vec();

        let writer = DumpWriter::spawn_sanitized(path.clone());
        writer.sender().send(Record {
            ts_ms: 1,
            service_uuid: bpsr_protocol::frame::SERVICE_UUID,
            method_id: bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
            payload,
            payload_decoded: true,
        });
        writer.shutdown();

        let contents = fs::read_to_string(&path).expect("dump file should exist");
        assert!(
            !contents.contains("TotallyRealPlayerName"),
            "the raw name must never reach disk when sanitize is on"
        );
        // The hex-encoded payload still round-trips to a `PlayerNNNNN`
        // placeholder — this isn't just an empty/dropped record.
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record = bpsr_protocol::dump_format::parse_record(lines[0]).unwrap();
        let decoded =
            bpsr_protocol::pb::SyncContainerData::decode(record.payload.as_slice()).unwrap();
        let name = decoded.v_data.unwrap().char_base.unwrap().name;
        assert!(name.starts_with("Player"));

        let _ = fs::remove_file(&path);
    }

    /// An unmodeled opcode (nothing `pb.rs` knows how to whitelist-by-
    /// re-encode) must be dropped entirely rather than written raw when
    /// sanitize is on — the same safety property `sanitize-dump` enforces
    /// offline.
    #[test]
    fn spawn_sanitized_drops_unmodeled_records() {
        let path = temp_path("sanitized-unmodeled");
        let writer = DumpWriter::spawn_sanitized(path.clone());
        let sender = writer.sender();
        sender.send(Record {
            ts_ms: 1,
            service_uuid: 0xDEAD,
            method_id: 0x1234, // not one of the seven modeled opcodes
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
            payload_decoded: true,
        });
        writer.shutdown();

        let contents = fs::read_to_string(&path).expect("dump file should exist");
        assert!(
            contents.is_empty(),
            "an unmodeled record must be dropped, not written raw"
        );
        assert_eq!(sender.sanitized_out_count(), 1);
        assert_eq!(sender.dropped_count(), 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn writer_creates_missing_parent_directory() {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-inspect-test-nested-{}-{n}",
            std::process::id()
        ));
        let path = dir.join("dump.jsonl");

        let writer = DumpWriter::spawn(path.clone());
        writer.sender().send(Record {
            ts_ms: 1,
            service_uuid: 1,
            method_id: 1,
            payload: vec![],
            payload_decoded: true,
        });
        writer.shutdown();

        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_line_round_trips_through_serde_json_independently() {
        let path = temp_path("roundtrip");
        let writer = DumpWriter::spawn(path.clone());
        writer.sender().send(Record {
            ts_ms: 42,
            service_uuid: 0x1122,
            method_id: 0x33,
            payload: vec![1, 2, 3],
            payload_decoded: true,
        });
        writer.shutdown();

        let file = File::open(&path).unwrap();
        let reader = std::io::BufReader::new(file);
        let mut count = 0;
        for line in reader.lines() {
            let line = line.unwrap();
            let _: serde_json::Value =
                serde_json::from_str(&line).expect("each line must be standalone valid JSON");
            count += 1;
        }
        assert_eq!(count, 1);
        let _ = fs::remove_file(&path);
    }

    /// A full channel drops the record and counts it instead of blocking:
    /// the capture/decode hot path must never wait on a stalled disk, so the
    /// dump goes incomplete — loudly, via `shutdown`'s log — rather than the
    /// decoder going quiet.
    #[test]
    fn a_full_channel_drops_records_and_counts_them_instead_of_blocking() {
        let (tx, _rx) = crossbeam_channel::bounded::<Record>(1);
        let sender = RecordSender::new(tx);

        for ts_ms in 0..3 {
            sender.send(Record {
                ts_ms,
                service_uuid: 1,
                method_id: 1,
                payload: vec![],
                payload_decoded: true,
            });
        }

        assert_eq!(
            sender.dropped.load(Ordering::Relaxed),
            2,
            "only the first record fits"
        );
    }

    /// `shutdown` joins successfully once every sender clone (not just the
    /// writer's own) has been dropped first — the normal shutdown sequence
    /// an `InspectSink` implementation must follow.
    #[test]
    fn writer_thread_exits_once_every_sender_clone_is_dropped_first() {
        let path = temp_path("exit");
        let writer = DumpWriter::spawn(path.clone());
        let extra = writer.sender();
        drop(extra);
        writer.shutdown();
        let _ = fs::remove_file(&path);
    }

    // -- rotation (issue #322: a numbered ring — dump-<session>.jsonl, .1,
    // .2, ... — replaced the old fixed-size, one-previous-file cap so a
    // long raid no longer loses everything but its last few minutes) ------

    fn line_len(record: &Record) -> u64 {
        serde_json::to_string(&Line::from(record)).unwrap().len() as u64 + 1
    }

    /// The threshold is crossed by a running writer thread, not just found
    /// crossed at startup — mirrors `logging::Tee`'s same requirement for a
    /// long-lived process.
    #[test]
    fn writer_rotates_when_the_running_total_crosses_the_threshold() {
        let path = temp_path("rotate");
        let rotated = numbered_sibling(&path, 1);
        let record1 = Record {
            ts_ms: 1,
            service_uuid: 0xAB,
            method_id: 2,
            payload: vec![0xDE, 0xAD],
            payload_decoded: true,
        };
        let record2 = Record {
            ts_ms: 2,
            service_uuid: 0xCD,
            method_id: 3,
            payload: vec![],
            payload_decoded: false,
        };
        let max_bytes = line_len(&record1) + line_len(&record2);

        let writer = DumpWriter::spawn_with_max_bytes(path.clone(), max_bytes, u64::MAX, false);
        writer.sender().send(record1);
        writer.sender().send(record2);
        writer.shutdown();

        let rotated_contents = fs::read_to_string(&rotated).expect("rotated file should exist");
        assert_eq!(rotated_contents.lines().count(), 2);
        assert_eq!(
            fs::read_to_string(&path).expect("path should be reopened empty"),
            ""
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    /// The file is opened in append mode, so a previous session's records
    /// count toward the threshold too — mirrors
    /// `logging::tee_seeds_its_count_from_the_file_it_appends_to`.
    #[test]
    fn writer_seeds_its_running_total_from_an_existing_dump_file() {
        let path = temp_path("seed");
        let rotated = numbered_sibling(&path, 1);
        fs::write(&path, b"previous-session-line\n").unwrap();
        let existing_len = fs::metadata(&path).unwrap().len();

        let record = Record {
            ts_ms: 1,
            service_uuid: 1,
            method_id: 1,
            payload: vec![],
            payload_decoded: true,
        };
        let max_bytes = existing_len + line_len(&record);

        let writer = DumpWriter::spawn_with_max_bytes(path.clone(), max_bytes, u64::MAX, false);
        writer.sender().send(record);
        writer.shutdown();

        let rotated_contents = fs::read(&rotated).expect("rotated file should exist");
        assert!(rotated_contents.starts_with(b"previous-session-line\n"));
        assert_eq!(
            fs::read_to_string(&path).expect("path should be reopened empty"),
            ""
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    /// Three chunk-sized rotations in a row must shift the whole ring, not
    /// just overwrite a single `.1` — `.1` always ends up the newest
    /// rotated-out chunk and `.3` the oldest, in that order.
    #[test]
    fn rotation_shifts_older_numbered_chunks_up_by_one() {
        let path = temp_path("ring-shift");
        let record = |ts_ms: u64| Record {
            ts_ms,
            service_uuid: 1,
            method_id: 1,
            payload: vec![0xAA],
            payload_decoded: true,
        };
        let max_bytes = line_len(&record(1));

        let writer = DumpWriter::spawn_with_max_bytes(path.clone(), max_bytes, u64::MAX, false);
        writer.sender().send(record(1));
        writer.sender().send(record(2));
        writer.sender().send(record(3));
        writer.shutdown();

        let dot1 = fs::read_to_string(numbered_sibling(&path, 1)).expect(".1 should exist");
        let dot2 = fs::read_to_string(numbered_sibling(&path, 2)).expect(".2 should exist");
        let dot3 = fs::read_to_string(numbered_sibling(&path, 3)).expect(".3 should exist");
        assert!(dot1.contains("\"ts_ms\":3"), "newest rotated chunk is .1");
        assert!(dot2.contains("\"ts_ms\":2"), "middle chunk shifted to .2");
        assert!(dot3.contains("\"ts_ms\":1"), "oldest chunk shifted to .3");
        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(numbered_sibling(&path, 1));
        let _ = fs::remove_file(numbered_sibling(&path, 2));
        let _ = fs::remove_file(numbered_sibling(&path, 3));
    }

    /// Once the ring's total size exceeds the byte budget, the oldest
    /// (highest-numbered) chunk is deleted rather than kept forever —
    /// issue #322's bounded replacement for the unbounded-disk-use
    /// alternative.
    #[test]
    fn rotation_deletes_the_oldest_chunk_once_the_ring_exceeds_its_byte_budget() {
        let path = temp_path("ring-budget");
        let record = |ts_ms: u64| Record {
            ts_ms,
            service_uuid: 1,
            method_id: 1,
            payload: vec![0xAA],
            payload_decoded: true,
        };
        let chunk_bytes = line_len(&record(1));
        // Room for exactly two rotated chunks; a third rotation must evict
        // the oldest one.
        let total_bytes = chunk_bytes * 2;

        let writer =
            DumpWriter::spawn_with_max_bytes(path.clone(), chunk_bytes, total_bytes, false);
        writer.sender().send(record(1));
        writer.sender().send(record(2));
        writer.sender().send(record(3));
        writer.shutdown();

        assert!(
            numbered_sibling(&path, 1).exists(),
            ".1 (newest rotated chunk) must survive"
        );
        assert!(
            numbered_sibling(&path, 2).exists(),
            ".2 must survive — it still fits the budget"
        );
        assert!(
            !numbered_sibling(&path, 3).exists(),
            ".3 (oldest) must be deleted once the ring goes over budget"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(numbered_sibling(&path, 1));
        let _ = fs::remove_file(numbered_sibling(&path, 2));
    }

    /// Regression test for the review finding on PR #99: the failure arm
    /// used to leave `written` untouched, so once rotation started failing
    /// (locked `.1`, permission denial, a read-only/full volume) *every*
    /// subsequent record would retry the failing rename and warn, instead
    /// of the documented once-per-`max_bytes` backoff that
    /// `logging::Tee::rotate` gets by unconditionally zeroing
    /// `self.written`. Both arms must reset the count.
    #[test]
    fn rotation_outcome_resets_written_on_success_and_on_failure() {
        let success_path = temp_path("rotation-outcome-success");
        let out = BufWriter::new(File::create(&success_path).unwrap());
        let fresh_path = temp_path("rotation-outcome-fresh");
        let fresh_file = File::create(&fresh_path).unwrap();

        let (_out, written) = rotation_outcome(out, Some(fresh_file));

        assert_eq!(written, 0, "a successful rotation resets written to 0");
        let _ = fs::remove_file(&success_path);
        let _ = fs::remove_file(&fresh_path);

        let failure_path = temp_path("rotation-outcome-failure");
        let out = BufWriter::new(File::create(&failure_path).unwrap());

        let (_out, written) = rotation_outcome(out, None);

        assert_eq!(
            written, 0,
            "a failed rotation must also reset written to 0, mirroring \
             logging::Tee::rotate, so the failing rename is retried once \
             per max_bytes rather than on every subsequent record"
        );
        let _ = fs::remove_file(&failure_path);
    }

    /// Companion to the test above, for the *other* piece of per-chunk
    /// state a rotation attempt updates. `written` resets on both arms;
    /// `first_ts_ms` must not. A failed rotation keeps appending to the same
    /// file, so clearing the chunk's start timestamp would make the next
    /// successful rotation log a `ts_ms` range that starts after the
    /// failure and understates what the chunk actually holds.
    #[test]
    fn next_first_ts_ms_clears_only_after_a_rotation_that_succeeded() {
        assert_eq!(
            next_first_ts_ms(true, Some(1_000)),
            None,
            "a successful rotation starts a new chunk, so the range restarts"
        );
        assert_eq!(
            next_first_ts_ms(false, Some(1_000)),
            Some(1_000),
            "a failed rotation keeps writing the same chunk, so its start \
             timestamp must survive"
        );
        assert_eq!(next_first_ts_ms(false, None), None);
    }

    /// Regression test for the review finding on PR #325: a failed shift
    /// used to warn and carry on, so the next rename in the loop overwrote
    /// the chunk that had just failed to move. The shift now stops at the
    /// first failure and reports it, and `rotate` skips the rotation
    /// entirely rather than renaming the live file onto a ring it couldn't
    /// vacate. Simulated by putting a directory where `.2` wants to move:
    /// renaming a file onto an existing directory fails on every platform,
    /// and a directory is not itself a ring chunk, so it doesn't shift.
    #[test]
    fn a_failed_ring_shift_aborts_the_rotation_instead_of_clobbering_a_chunk() {
        let path = temp_path("shift-abort");
        let dot1 = numbered_sibling(&path, 1);
        let dot2 = numbered_sibling(&path, 2);
        let blocked = numbered_sibling(&path, 3);
        fs::write(&path, b"live").unwrap();
        fs::write(&dot1, b"chunk-one").unwrap();
        fs::write(&dot2, b"chunk-two").unwrap();
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("occupant"), b"x").unwrap();

        assert!(!shift_ring(&path), "the shift must report its failure");
        assert!(
            rotate(&path, u64::MAX).is_none(),
            "a rotation that can't vacate .1 must not touch the live file"
        );

        assert_eq!(
            fs::read_to_string(&dot2).unwrap(),
            "chunk-two",
            ".2 could not move, so .1 must not have been renamed on top of it"
        );
        assert_eq!(fs::read_to_string(&dot1).unwrap(), "chunk-one");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "live",
            "the live file stays put and is retried on the next threshold crossing"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&dot1);
        let _ = fs::remove_file(&dot2);
        let _ = fs::remove_dir_all(&blocked);
    }

    /// A gap in the numbering (here: `.1` deleted out from under the ring)
    /// must not hide the chunks behind it from the byte budget — before the
    /// enumeration became gap-tolerant those chunks were orphaned, counted
    /// against nothing and evicted never.
    #[test]
    fn the_ring_budget_still_sees_chunks_behind_a_missing_number() {
        let path = temp_path("budget-gap");
        fs::write(&path, b"live").unwrap();
        let dot2 = numbered_sibling(&path, 2);
        let dot3 = numbered_sibling(&path, 3);
        fs::write(&dot2, b"0123456789").unwrap();
        fs::write(&dot3, b"0123456789").unwrap();

        // Room for one 10-byte chunk only: the oldest (.3) has to go.
        enforce_ring_budget(&path, 10);

        assert!(dot2.exists(), ".2 must survive — it fits the budget");
        assert!(
            !dot3.exists(),
            ".3 must be evicted; a vacant .1 must not hide it from the budget"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&dot2);
        let _ = fs::remove_file(&dot3);
    }

    /// A dump file already at or above the threshold at process start is
    /// rotated before the first record is ever written to it.
    #[test]
    fn writer_rotates_a_pre_existing_oversized_file_at_startup() {
        let path = temp_path("startup-rotate");
        let rotated = numbered_sibling(&path, 1);
        fs::write(&path, b"stale-oversized-content").unwrap();
        let max_bytes = fs::metadata(&path).unwrap().len();

        let writer = DumpWriter::spawn_with_max_bytes(path.clone(), max_bytes, u64::MAX, false);
        writer.shutdown();

        assert_eq!(
            fs::read(&rotated).expect("rotated file should exist"),
            b"stale-oversized-content"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    // -- max_total_ring_bytes_from ------------------------------------------

    #[test]
    fn max_total_ring_bytes_from_prefers_a_valid_positive_override() {
        assert_eq!(max_total_ring_bytes_from(Some("1024")), 1024);
    }

    #[test]
    fn max_total_ring_bytes_from_falls_back_to_the_default_when_unset_zero_or_unparseable() {
        assert_eq!(
            max_total_ring_bytes_from(None),
            DEFAULT_MAX_TOTAL_RING_BYTES
        );
        assert_eq!(
            max_total_ring_bytes_from(Some("0")),
            DEFAULT_MAX_TOTAL_RING_BYTES
        );
        assert_eq!(
            max_total_ring_bytes_from(Some("not-a-number")),
            DEFAULT_MAX_TOTAL_RING_BYTES
        );
    }

    // -- sweep_prior_sessions (issue #346): the prior-session dump sweep. --

    fn sweep_test_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-inspect-sweep-{tag}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file_with_len(path: &Path, len: usize) {
        fs::write(path, vec![b'a'; len]).unwrap();
    }

    fn set_mtime(path: &Path, age: std::time::Duration) {
        let when = std::time::SystemTime::now()
            .checked_sub(age)
            .expect("age should be representable");
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn sweep_prior_sessions_removes_a_prior_session_file_older_than_max_age() {
        let dir = sweep_test_dir("age");
        let current = dir.join("dump-100-1000.jsonl");
        write_file_with_len(&current, 10);

        let old = dir.join("dump-200-2000.jsonl");
        write_file_with_len(&old, 10);
        set_mtime(&old, std::time::Duration::from_secs(8 * 24 * 3600));

        sweep_prior_sessions(
            &current,
            u64::MAX,
            std::time::Duration::from_secs(7 * 24 * 3600),
        );

        assert!(current.exists());
        assert!(!old.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_prior_sessions_evicts_over_budget_prior_sessions_oldest_first() {
        let dir = sweep_test_dir("budget");
        let current = dir.join("dump-100-1000.jsonl");
        write_file_with_len(&current, 10);

        let older = dir.join("dump-200-2000.jsonl");
        write_file_with_len(&older, 10);
        set_mtime(&older, std::time::Duration::from_secs(60));

        let newer = dir.join("dump-300-3000.jsonl");
        write_file_with_len(&newer, 10);
        set_mtime(&newer, std::time::Duration::from_secs(30));

        // Budget only fits one of the two 10-byte prior-session files.
        sweep_prior_sessions(&current, 10, std::time::Duration::from_secs(7 * 24 * 3600));

        assert!(current.exists());
        assert!(
            !older.exists(),
            "the older-mtime file should be evicted first"
        );
        assert!(newer.exists(), "the newer-mtime file should survive");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_prior_sessions_never_touches_current_chunks_or_non_matching_names() {
        let dir = sweep_test_dir("current");
        let current = dir.join("dump-100-1000.jsonl");
        write_file_with_len(&current, 10);
        let current_chunk = numbered_sibling(&current, 1);
        write_file_with_len(&current_chunk, 10);
        set_mtime(
            &current_chunk,
            std::time::Duration::from_secs(8 * 24 * 3600),
        );

        let unrelated = dir.join("not-a-dump-file.txt");
        write_file_with_len(&unrelated, 10);
        set_mtime(&unrelated, std::time::Duration::from_secs(8 * 24 * 3600));

        // Zero budget and zero max-age would evict everything eligible.
        sweep_prior_sessions(&current, 0, std::time::Duration::from_secs(0));

        assert!(current.exists());
        assert!(current_chunk.exists());
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // -- rotation log signal (issue #322: a routine rotation used to leave
    // no trace at all — see the module-level `diagnostics` submodule for
    // the shared log-capture harness and its "positive assertions only"
    // discipline, since `log` allows exactly one logger per test binary). -

    mod diagnostics {
        use super::*;
        use std::sync::{Mutex, Once};

        static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

        struct CaptureLogger;

        impl log::Log for CaptureLogger {
            fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
                true
            }

            fn log(&self, record: &log::Record<'_>) {
                if let Ok(mut captured) = CAPTURED.lock() {
                    captured.push(record.args().to_string());
                }
            }

            fn flush(&self) {}
        }

        /// Installs [`CAPTURE_LOGGER`] once per process. Idempotent, so any
        /// number of tests can call it, in any order, from any thread.
        fn install_capture() {
            static INSTALL: Once = Once::new();
            INSTALL.call_once(|| {
                let _ = log::set_logger(&CAPTURE_LOGGER);
                log::set_max_level(log::LevelFilter::Trace);
            });
        }

        /// Whether any captured line contains every one of `needles` —
        /// callers pass a value unique to their test (a distinct `ts_ms` or
        /// temp path) as one of the needles, so a match can only have come
        /// from that test, never a different test sharing this
        /// process-wide capture buffer.
        fn logged(needles: &[&str]) -> bool {
            CAPTURED
                .lock()
                .map(|captured| {
                    captured
                        .iter()
                        .any(|line| needles.iter().all(|needle| line.contains(needle)))
                })
                .unwrap_or(false)
        }

        /// A successful rotation logs the bytes written and the first/last
        /// `ts_ms` of the chunk being rotated out — tracked by `run_writer`
        /// as records are written, never by re-reading the rotated file.
        #[test]
        fn rotation_logs_bytes_written_and_the_rotated_chunks_ts_ms_range() {
            install_capture();
            let path = temp_path("log-rotate");
            let record = |ts_ms: u64| Record {
                ts_ms,
                service_uuid: 1,
                method_id: 1,
                payload: vec![0xAA],
                payload_decoded: true,
            };
            // Unique, unmistakable ts_ms values so the assertion below can
            // only match lines this test itself produced.
            let (first_ts, last_ts) = (932_411_001_u64, 932_411_002_u64);
            let max_bytes = line_len(&record(first_ts)) + line_len(&record(last_ts));

            let writer = DumpWriter::spawn_with_max_bytes(path.clone(), max_bytes, u64::MAX, false);
            writer.sender().send(record(first_ts));
            writer.sender().send(record(last_ts));
            writer.shutdown();

            assert!(
                logged(&[
                    "inspect dump rotated",
                    &first_ts.to_string(),
                    &last_ts.to_string()
                ]),
                "expected a rotation log line naming both ts_ms bounds; captured: {:?}",
                CAPTURED.lock().unwrap()
            );

            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(numbered_sibling(&path, 1));
        }

        /// The writer logs which path it's using and the retention policy
        /// (chunk size + ring budget) once at startup, not only on failure.
        #[test]
        fn writer_logs_its_path_and_retention_policy_at_startup() {
            install_capture();
            let path = temp_path("log-startup");
            let chunk_bytes = 123_456_u64;
            let total_bytes = 654_321_u64;

            let writer =
                DumpWriter::spawn_with_max_bytes(path.clone(), chunk_bytes, total_bytes, false);
            writer.shutdown();

            assert!(
                logged(&[
                    "inspect dump writer started",
                    &path.display().to_string(),
                    &chunk_bytes.to_string(),
                    &total_bytes.to_string(),
                ]),
                "expected a startup log line naming the path and retention policy; captured: {:?}",
                CAPTURED.lock().unwrap()
            );

            let _ = fs::remove_file(&path);
        }

        /// Deleting the oldest ring chunk once the budget is exceeded is
        /// itself logged, naming the deleted chunk's path.
        #[test]
        fn budget_eviction_logs_the_deleted_chunk_path() {
            install_capture();
            let path = temp_path("log-evict");
            let record = |ts_ms: u64| Record {
                ts_ms,
                service_uuid: 1,
                method_id: 1,
                payload: vec![0xAA],
                payload_decoded: true,
            };
            let chunk_bytes = line_len(&record(1));
            let total_bytes = chunk_bytes; // room for exactly one rotated chunk

            let writer =
                DumpWriter::spawn_with_max_bytes(path.clone(), chunk_bytes, total_bytes, false);
            writer.sender().send(record(1));
            writer.sender().send(record(2));
            writer.shutdown();

            let evicted = numbered_sibling(&path, 2).display().to_string();
            assert!(
                logged(&["deleted oldest chunk", &evicted]),
                "expected an eviction log line naming the deleted chunk; captured: {:?}",
                CAPTURED.lock().unwrap()
            );
            assert!(!numbered_sibling(&path, 2).exists());

            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(numbered_sibling(&path, 1));
        }
    }
}
