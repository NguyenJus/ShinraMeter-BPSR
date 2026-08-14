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
//! object with exactly these four fields:
//!
//! ```json
//! {"ts_ms":1699999999999,"service_uuid":"0x0000000063335342","method_id":"0x0000002d","payload_hex":"0a1b2c"}
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
//! - `payload_hex` — the fragment's payload **after** zstd decompression
//!   (i.e. exactly the bytes `bpsr_protocol::decode::decode_notify` would
//!   have decoded) — lowercase hex, two characters per byte, no separators
//!   or prefix. Empty string for a zero-length payload.
//!
//! Every field is a JSON string except `ts_ms`, specifically so a
//! `service_uuid`/`method_id` value is never subject to `f64` precision loss
//! the way a bare large-integer JSON number could be.
//!
//! One record is written per `bpsr_protocol::InspectSink::on_notify` call —
//! for *every* service uuid seen, not only the recognized one — which is
//! what makes this dump sufficient for slice B to rebuild service/method/attr
//! id histograms without a live game session.
//!
//! Blocking file IO happens entirely on a dedicated writer thread fed over a
//! channel (mirrors `crate::settings::spawn_writer`'s dedicated-writer-thread
//! shape, adapted here to write every record instead of coalescing to the
//! latest one — a dump needs every frame, not just the newest), so it never
//! sits on the capture/decode hot path.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::Serialize;

/// One dumped Notify-shaped fragment, in memory. See the module doc comment
/// for the exact on-disk JSON shape this serializes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub ts_ms: u64,
    pub service_uuid: u64,
    pub method_id: u32,
    pub payload: Vec<u8>,
}

/// The on-disk shape of one `Record` — see the module doc comment.
#[derive(Serialize)]
struct Line {
    ts_ms: u64,
    service_uuid: String,
    method_id: String,
    payload_hex: String,
}

impl From<&Record> for Line {
    fn from(r: &Record) -> Self {
        Self {
            ts_ms: r.ts_ms,
            service_uuid: format!("0x{:016x}", r.service_uuid),
            method_id: format!("0x{:08x}", r.method_id),
            payload_hex: r.payload.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

/// Sending half plus the writer-thread join handle. `spawn` opens (creating
/// missing parent directories) and owns the dump file for the life of the
/// thread; every `Record` sent over the channel is appended as one line.
pub struct DumpWriter {
    tx: Sender<Record>,
    handle: Option<JoinHandle<()>>,
}

impl DumpWriter {
    /// Spawns the dedicated writer thread.
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = unbounded::<Record>();
        let handle = std::thread::Builder::new()
            .name("inspect-dump-writer".to_string())
            .spawn(move || run_writer(rx, &path))
            .expect("failed to spawn the inspect-dump-writer thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// A cloneable sender for feeding records to the writer thread.
    pub fn sender(&self) -> Sender<Record> {
        self.tx.clone()
    }

    /// Drops this writer's own sender and blocks until the writer thread has
    /// drained (and written) every record already in flight. Note: any other
    /// clone of `sender()` still alive elsewhere (e.g. held by an
    /// `InspectSink` implementation) must be dropped too before the writer
    /// thread can actually exit — the channel only closes once every sender
    /// is gone.
    pub fn shutdown(mut self) {
        let handle = self.handle.take();
        drop(self);
        if let Some(handle) = handle {
            let _ = handle.join();
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
        writer
            .sender()
            .send(Record {
                ts_ms: 1,
                service_uuid: 0xAB,
                method_id: 0x02,
                payload: vec![0xDE, 0xAD],
            })
            .unwrap();
        writer
            .sender()
            .send(Record {
                ts_ms: 2,
                service_uuid: 0xCD,
                method_id: 0x03,
                payload: vec![],
            })
            .unwrap();
        writer.shutdown();

        let contents = fs::read_to_string(&path).expect("dump file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"{"ts_ms":1,"service_uuid":"0x00000000000000ab","method_id":"0x00000002","payload_hex":"dead"}"#
        );
        assert_eq!(
            lines[1],
            r#"{"ts_ms":2,"service_uuid":"0x00000000000000cd","method_id":"0x00000003","payload_hex":""}"#
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
        writer
            .sender()
            .send(Record {
                ts_ms: 1,
                service_uuid: 1,
                method_id: 1,
                payload: vec![],
            })
            .unwrap();
        writer.shutdown();

        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_line_round_trips_through_serde_json_independently() {
        let path = temp_path("roundtrip");
        let writer = DumpWriter::spawn(path.clone());
        writer
            .sender()
            .send(Record {
                ts_ms: 42,
                service_uuid: 0x1122,
                method_id: 0x33,
                payload: vec![1, 2, 3],
            })
            .unwrap();
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
