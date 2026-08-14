//! Opt-in packet-inspection diagnostic mode (issue #25 slice A).
//!
//! Off by default — `SHINRA_INSPECT=1` (any non-empty value other than `0`
//! or `false`, case-insensitively) turns it on. When on:
//!
//! - every Notify-shaped fragment observed — recognized service or not — is
//!   appended to a JSONL dump file on a dedicated writer thread, so a
//!   session can be replayed offline (slice B item 4). See [`crate::dump`]
//!   for the exact on-disk format.
//! - the first time a given unrecognized service uuid is seen it is logged
//!   at `info` level with its method id, payload length, and a truncated
//!   hex prefix; a running count and first-seen timestamp are kept and
//!   logged again in a summary when diagnostics shut down (slice A item 1).
//! - the first time an attr id with no known constant is seen for a given
//!   entity uid it is logged the same way, keyed by uid (slice A item 3).
//!
//! The dump path defaults to `%APPDATA%\shinra-bpsr\inspect\dump-<pid>.jsonl`
//! (or `shinra-bpsr-inspect-dump-<pid>.jsonl` in the working directory if
//! `APPDATA` is unset — e.g. this Linux dev host), overridable with
//! `SHINRA_INSPECT_DUMP=<path>`.
//!
//! Dumps contain player names and other identifying traffic — never attach
//! one to an issue or PR (see `.gitignore`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bpsr_protocol::InspectSink;
use crossbeam_channel::Sender;

use crate::dump;

/// How many leading bytes of an observed payload get hex-logged. Diagnostic
/// logging, not the dump file (which keeps every byte) — this just keeps
/// log lines readable.
const HEX_PREFIX_LEN: usize = 32;

/// True when `SHINRA_INSPECT` turns diagnostics on.
pub fn enabled() -> bool {
    enabled_from(std::env::var("SHINRA_INSPECT").ok().as_deref())
}

fn enabled_from(var: Option<&str>) -> bool {
    match var {
        Some(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        None => false,
    }
}

/// Where the raw frame dump is written. See the module doc comment for the
/// default and the `SHINRA_INSPECT_DUMP` override.
fn dump_path() -> PathBuf {
    dump_path_from(
        std::env::var("SHINRA_INSPECT_DUMP").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
    )
}

fn dump_path_from(inspect_dump: Option<&str>, appdata: Option<&str>) -> PathBuf {
    if let Some(path) = inspect_dump
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    match appdata {
        Some(appdata) if !appdata.is_empty() => PathBuf::from(appdata)
            .join("shinra-bpsr")
            .join("inspect")
            .join(format!("dump-{}.jsonl", std::process::id())),
        _ => {
            log::warn!("APPDATA is not set; falling back to a working-directory dump file");
            PathBuf::from(format!(
                "shinra-bpsr-inspect-dump-{}.jsonl",
                std::process::id()
            ))
        }
    }
}

/// Returned by [`init`] when diagnostics are on: the sink to hand to
/// `bpsr_capture::start_capture`, plus the means to shut the dump-writer
/// thread down cleanly at process exit.
pub struct Handle {
    pub sink: Arc<dyn InspectSink>,
    writer: dump::DumpWriter,
}

impl Handle {
    /// Drops this handle's own reference to the sink (a summary of what it
    /// observed is logged when the *last* reference — including the one the
    /// capture thread's `Decoder` holds — is dropped; see
    /// `DiagnosticSink`'s `Drop` impl), then flushes and joins the
    /// dump-writer thread. Call `bpsr_capture::CaptureHandle::stop` (or
    /// otherwise ensure the capture thread has exited) before this, so the
    /// capture thread's own sink reference is already gone and the summary
    /// actually gets logged here rather than staying pinned open.
    pub fn shutdown(self) {
        drop(self.sink);
        self.writer.shutdown();
    }
}

/// Turns diagnostics on if `SHINRA_INSPECT` is set, spawning the dump-writer
/// thread and returning a `Handle`; `None` (zero cost beyond the env check)
/// otherwise.
pub fn init() -> Option<Handle> {
    if !enabled() {
        return None;
    }
    let path = dump_path();
    log::info!(
        "packet inspection enabled (SHINRA_INSPECT set); dumping to {}",
        path.display()
    );
    let writer = dump::DumpWriter::spawn(path);
    let sink: Arc<dyn InspectSink> = Arc::new(DiagnosticSink::new(writer.sender()));
    Some(Handle { sink, writer })
}

#[derive(Debug, Clone)]
struct ServiceStat {
    count: u64,
    first_seen_ms: u64,
    last_method_id: u32,
    last_payload_len: usize,
    hex_prefix: String,
}

/// The real `InspectSink`: forwards every observation to the dump-writer
/// channel, and keeps an in-memory tally of unrecognized service ids and
/// unknown attr ids for the log-on-first-sight-plus-shutdown-summary
/// behavior described in the module doc comment.
struct DiagnosticSink {
    tx: Sender<dump::Record>,
    services: Mutex<HashMap<u64, ServiceStat>>,
    attrs: Mutex<HashMap<(i64, i32), u64>>,
}

impl DiagnosticSink {
    fn new(tx: Sender<dump::Record>) -> Self {
        Self {
            tx,
            services: Mutex::new(HashMap::new()),
            attrs: Mutex::new(HashMap::new()),
        }
    }

    fn log_summary(&self) {
        for (uuid, stat) in self.services.lock().unwrap().iter() {
            log::info!(
                "packet-inspect summary: unrecognized service_uuid=0x{uuid:016x} count={} first_seen_ms={} last_method_id=0x{:08x} last_payload_len={} hex_prefix={}",
                stat.count,
                stat.first_seen_ms,
                stat.last_method_id,
                stat.last_payload_len,
                stat.hex_prefix,
            );
        }
        for ((uid, attr_id), count) in self.attrs.lock().unwrap().iter() {
            log::info!(
                "packet-inspect summary: unknown attr_id=0x{attr_id:x} uid={uid} count={count}"
            );
        }
    }
}

/// Logs the aggregated summary once the last reference to this sink is
/// dropped (see `Handle::shutdown`) — so a session's unrecognized-service
/// and unknown-attr findings are never lost even if nothing else ever asks
/// for them mid-run.
impl Drop for DiagnosticSink {
    fn drop(&mut self) {
        self.log_summary();
    }
}

impl InspectSink for DiagnosticSink {
    fn on_notify(&self, service_uuid: u64, method_id: u32, payload: &[u8], now_ms: u64) {
        // Every service uuid, recognized or not, feeds the raw dump — it's
        // what makes slice B's offline replay possible.
        let _ = self.tx.send(dump::Record {
            ts_ms: now_ms,
            service_uuid,
            method_id,
            payload: payload.to_vec(),
        });

        if service_uuid == bpsr_protocol::frame::SERVICE_UUID {
            return;
        }
        let mut services = self.services.lock().unwrap();
        let is_new = !services.contains_key(&service_uuid);
        let stat = services.entry(service_uuid).or_insert_with(|| ServiceStat {
            count: 0,
            first_seen_ms: now_ms,
            last_method_id: method_id,
            last_payload_len: payload.len(),
            hex_prefix: hex_prefix(payload),
        });
        stat.count += 1;
        stat.last_method_id = method_id;
        stat.last_payload_len = payload.len();
        if is_new {
            log::info!(
                "packet-inspect: new unrecognized service_uuid=0x{service_uuid:016x} method_id=0x{method_id:08x} payload_len={} first_seen_ms={now_ms} hex_prefix={}",
                payload.len(),
                stat.hex_prefix,
            );
        }
    }

    fn on_attr(&self, uid: i64, attr_id: i32, raw: &[u8], known: bool) {
        // A known id is already decoded elsewhere and isn't a discovery —
        // this sink only aggregates/logs the unrecognized ones (slice A's
        // behavior, preserved as-is after slice B widened the seam itself
        // to report every id).
        if known {
            return;
        }
        let mut attrs = self.attrs.lock().unwrap();
        let count = attrs.entry((uid, attr_id)).or_insert(0);
        *count += 1;
        if *count == 1 {
            log::info!(
                "packet-inspect: new unknown attr_id=0x{attr_id:x} uid={uid} raw_hex={}",
                hex_prefix(raw),
            );
        }
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(HEX_PREFIX_LEN)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- enabled ------------------------------------------------------

    #[test]
    fn enabled_is_false_when_unset() {
        assert!(!enabled_from(None));
    }

    #[test]
    fn enabled_is_false_for_0_or_false_case_insensitive() {
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("false")));
        assert!(!enabled_from(Some("FALSE")));
        assert!(!enabled_from(Some("")));
    }

    #[test]
    fn enabled_is_true_for_1_or_any_other_nonempty_value() {
        assert!(enabled_from(Some("1")));
        assert!(enabled_from(Some("yes")));
        assert!(enabled_from(Some("true")));
    }

    // -- dump_path ------------------------------------------------------

    #[test]
    fn dump_path_prefers_the_explicit_override() {
        let path = dump_path_from(Some("/tmp/custom-dump.jsonl"), Some("/appdata"));
        assert_eq!(path, PathBuf::from("/tmp/custom-dump.jsonl"));
    }

    #[test]
    fn dump_path_falls_back_to_appdata_when_unset() {
        let path = dump_path_from(None, Some("/appdata"));
        assert!(path.starts_with("/appdata/shinra-bpsr/inspect"));
        assert!(path.to_string_lossy().ends_with(".jsonl"));
    }

    #[test]
    fn dump_path_falls_back_to_working_directory_when_neither_is_set() {
        let path = dump_path_from(None, None);
        assert!(
            path.to_string_lossy()
                .starts_with("shinra-bpsr-inspect-dump-")
        );
    }

    // -- DiagnosticSink ---------------------------------------------------

    fn new_sink() -> (DiagnosticSink, crossbeam_channel::Receiver<dump::Record>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (DiagnosticSink::new(tx), rx)
    }

    #[test]
    fn on_notify_forwards_every_service_to_the_dump_channel_in_order() {
        let (sink, rx) = new_sink();
        sink.on_notify(bpsr_protocol::frame::SERVICE_UUID, 1, b"a", 10);
        sink.on_notify(0xDEAD, 2, b"bb", 20);

        assert_eq!(
            rx.try_recv().unwrap(),
            dump::Record {
                ts_ms: 10,
                service_uuid: bpsr_protocol::frame::SERVICE_UUID,
                method_id: 1,
                payload: b"a".to_vec(),
            }
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            dump::Record {
                ts_ms: 20,
                service_uuid: 0xDEAD,
                method_id: 2,
                payload: b"bb".to_vec(),
            }
        );
    }

    #[test]
    fn unrecognized_service_is_aggregated_with_a_count_and_first_seen_timestamp() {
        let (sink, _rx) = new_sink();
        sink.on_notify(0xDEAD, 1, b"a", 10);
        sink.on_notify(0xDEAD, 2, b"bb", 20);

        let services = sink.services.lock().unwrap();
        let stat = services.get(&0xDEAD).expect("service should be tracked");
        assert_eq!(stat.count, 2);
        assert_eq!(stat.first_seen_ms, 10);
        assert_eq!(stat.last_method_id, 2);
        assert_eq!(stat.last_payload_len, 2);
    }

    #[test]
    fn recognized_service_is_not_aggregated_as_unrecognized() {
        let (sink, _rx) = new_sink();
        sink.on_notify(bpsr_protocol::frame::SERVICE_UUID, 1, b"a", 10);
        assert!(sink.services.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_attr_is_counted_per_uid_and_attr_id() {
        let (sink, _rx) = new_sink();
        sink.on_attr(5, 0x99, &[1, 2], false);
        sink.on_attr(5, 0x99, &[3], false);
        sink.on_attr(6, 0x99, &[], false);

        let attrs = sink.attrs.lock().unwrap();
        assert_eq!(attrs.get(&(5, 0x99)), Some(&2));
        assert_eq!(attrs.get(&(6, 0x99)), Some(&1));
    }

    #[test]
    fn known_attr_is_not_aggregated_or_logged_as_a_discovery() {
        let (sink, _rx) = new_sink();
        sink.on_attr(5, 0x99, &[1], true);

        assert!(sink.attrs.lock().unwrap().is_empty());
    }

    #[test]
    fn hex_prefix_truncates_long_payloads() {
        let payload = vec![0xAB; 100];
        assert_eq!(hex_prefix(&payload).len(), HEX_PREFIX_LEN * 2);
    }

    #[test]
    fn hex_prefix_of_empty_payload_is_empty_string() {
        assert_eq!(hex_prefix(&[]), "");
    }
}
