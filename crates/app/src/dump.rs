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
//! fragments whose payload would not decompress — which is what makes this
//! dump sufficient for slice B to rebuild service/method/attr id histograms
//! without a live game session.
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
}

impl RecordSender {
    /// Wraps a raw channel sender (the writer thread's, or a test's).
    pub fn new(tx: Sender<Record>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
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
}

/// Sending half plus the writer-thread join handle. `spawn` opens (creating
/// missing parent directories) and owns the dump file for the life of the
/// thread; every `Record` sent over the channel is appended as one line.
pub struct DumpWriter {
    tx: RecordSender,
    handle: Option<JoinHandle<()>>,
}

impl DumpWriter {
    /// Spawns the dedicated writer thread.
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = bounded::<Record>(CAPACITY);
        let handle = std::thread::Builder::new()
            .name("inspect-dump-writer".to_string())
            .spawn(move || run_writer(rx, &path))
            .expect("failed to spawn the inspect-dump-writer thread");
        Self {
            tx: RecordSender::new(tx),
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
        drop(self);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        let dropped = dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            log::warn!(
                "packet-inspect summary: inspect dump is INCOMPLETE — dropped {dropped} record(s), the writer thread could not keep up"
            );
        }
    }
}

/// Opens (or creates) the dump file for appending and writes each record it
/// receives as one JSONL line, flushing on exit. Never panics: if the file
/// can't be opened, drains the channel forever instead so senders never
/// block on a dead writer.
fn run_writer(rx: Receiver<Record>, path: &Path) {
    let file = match open(path) {
        Some(f) => f,
        None => {
            while rx.recv().is_ok() {}
            return;
        }
    };
    let mut out = BufWriter::new(file);
    while let Ok(record) = rx.recv() {
        let line = Line::from(&record);
        match serde_json::to_string(&line) {
            Ok(json) => {
                if let Err(err) = writeln!(out, "{json}") {
                    log::warn!("inspect dump write failed: {err}");
                }
            }
            Err(err) => log::warn!("inspect dump serialize failed: {err}"),
        }
    }
    let _ = out.flush();
}

fn open(path: &Path) -> Option<File> {
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
            "shinra-bpsr-inspect-test-{tag}-{}-{n}.jsonl",
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

    #[test]
    fn writer_creates_missing_parent_directory() {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "shinra-bpsr-inspect-test-nested-{}-{n}",
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
}
