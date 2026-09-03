//! Packet-inspection diagnostic mode (issue #25 slice A).
//!
//! Off by default (issue #122) — set `SHINRA_INSPECT=1` (`true`, `on`, or
//! `yes` also work, case-insensitively) to opt in. When on:
//!
//! - every Notify-shaped fragment observed — recognized service or not, and
//!   including one whose payload would not decompress (dumped as its raw
//!   bytes with `payload_decoded: false`) — is appended to a JSONL dump file
//!   on a dedicated writer thread, so a session can be replayed offline
//!   (slice B item 4). See [`crate::dump`] for the exact on-disk format, and
//!   for the drop-on-full policy that can make a dump incomplete under a
//!   stalled disk (reported at shutdown).
//! - the first time a given unrecognized service uuid is seen it is logged
//!   at `info` level with its method id, payload length, and a truncated
//!   hex prefix; a running count and first-seen timestamp are kept and
//!   logged again in a summary when diagnostics shut down (slice A item 1).
//! - the first time an attr id with no known constant is seen **at all** it
//!   is logged the same way (slice A item 3). This is keyed by `attr_id`
//!   alone, *not* by `(uid, attr_id)`: a real session has ~60 distinct
//!   unknown attr ids but thousands of entities, and keying discovery by uid
//!   turned "log a new fact" into "log the same ~60 facts once per entity",
//!   flooding the log file to eviction inside an hour with zero scene/boss
//!   diagnostics ever surviving (issue #69). Per-attr aggregates (total
//!   count, distinct uids seen up to a cap, a sample uid, and a sample hex
//!   prefix) are still kept and logged in the shutdown summary, one line per
//!   attr id.
//!
//! The dump path defaults to
//! `%APPDATA%\ShinraMeter-BPSR\inspect\dump-<session_id>.jsonl` (or
//! `ShinraMeter-BPSR-inspect-dump-<session_id>.jsonl` in the working
//! directory if `APPDATA` is unset — e.g. this Linux dev host), overridable
//! with `SHINRA_INSPECT_DUMP=<path>`. `<session_id>` is `<pid>-<unix start
//! seconds>` (issue #322, see `crate::logging::session_id`) rather than a
//! bare pid, and is printed in the startup log banner too, so a dump file
//! can always be matched back to the session log that produced it.
//!
//! The dump file is capped and rotated into a numbered ring —
//! `dump-<session_id>.jsonl`, `.1`, `.2`, ... — rather than growing
//! unbounded; see [`crate::dump`]'s module doc comment for the per-chunk
//! size, the total ring budget, and the `SHINRA_INSPECT_MAX_BYTES`
//! override.
//!
//! Dumps contain player names and other identifying traffic — never attach
//! one to an issue or PR (see `.gitignore`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use bpsr_protocol::InspectSink;

use crate::dump;

/// How many leading bytes of an observed payload get hex-logged. Diagnostic
/// logging, not the dump file (which keeps every byte) — this just keeps
/// log lines readable.
const HEX_PREFIX_LEN: usize = 32;

/// How many distinct uids an [`AttrStat`] tracks per attr id before it stops
/// inserting new ones (issue #69). The exact distinct-uid count past this
/// point isn't diagnostically interesting — knowing it's "a lot" is enough,
/// and an unbounded per-attr uid set would itself become an unbounded-memory
/// version of the same flood problem this whole fix exists to avoid. When
/// the cap is hit, `AttrStat::uids_saturated` is set so the summary line
/// says so instead of silently under-reporting.
const MAX_TRACKED_UIDS_PER_ATTR: usize = 16;

/// False unless `SHINRA_INSPECT` is set to an explicit opt-in value (`1`,
/// `true`, `on`, or `yes`, case-insensitively) — diagnostics are off by
/// default (issue #122).
pub fn enabled() -> bool {
    enabled_from(std::env::var("SHINRA_INSPECT").ok().as_deref())
}

fn enabled_from(var: Option<&str>) -> bool {
    match var {
        Some(v) => {
            v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("yes")
        }
        None => false,
    }
}

/// Where the raw frame dump is written. See the module doc comment for the
/// default and the `SHINRA_INSPECT_DUMP` override.
pub(crate) fn dump_path() -> PathBuf {
    dump_path_from(
        std::env::var("SHINRA_INSPECT_DUMP").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
    )
}

fn dump_path_from(inspect_dump: Option<&str>, appdata: Option<&str>) -> PathBuf {
    // Issue #322: keyed by session id (`<pid>-<unix start seconds>`, see
    // `crate::logging::session_id`), not a bare pid — a pid alone is
    // reused across runs and can't tell two sessions' dumps apart, and this
    // is also the id the startup log banner prints, so a dump on disk can
    // always be matched back to the session that produced it.
    let session_id = crate::logging::session_id();
    let (path, warning) = crate::paths::resolve(
        inspect_dump,
        appdata,
        &[
            "ShinraMeter-BPSR",
            "inspect",
            &format!("dump-{session_id}.jsonl"),
        ],
        &format!("ShinraMeter-BPSR-inspect-dump-{session_id}.jsonl"),
        "APPDATA is not set; falling back to a working-directory dump file",
    );
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }
    path
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

/// Process-wide handle to the current session's dump-writer drop counter
/// (the session-bundle export's manifest field), set once by [`init`]'s `Some`
/// branch — mirrors `logging::session_id`'s `OnceLock` shape. Reading it
/// through [`dropped_count`] rather than threading a live [`Handle`]
/// through `OverlayApp`/`ui::draw_header_menu`'s already-deep call chain
/// lets the "Export session bundle" menu item report how incomplete the
/// dump might be with no signature changes anywhere in `ui.rs`.
///
/// Only ever set from `init`'s `Some` branch, which nothing in this
/// module's own unit tests calls (they construct `DumpWriter`/
/// `DiagnosticSink` directly) — so it can never leak state between tests
/// or carry a stale value from a previous process.
static DROPPED_COUNTER: OnceLock<dump::RecordSender> = OnceLock::new();

/// Turns diagnostics on when opted in via `SHINRA_INSPECT`, spawning the
/// dump-writer thread and returning a `Handle`; `None` (zero cost beyond the
/// env check) unless opted in.
pub fn init() -> Option<Handle> {
    if !enabled() {
        return None;
    }
    let path = dump_path();
    log::info!(
        "packet inspection enabled via SHINRA_INSPECT; dumping to {}",
        path.display()
    );
    let writer = dump::DumpWriter::spawn(path);
    let _ = DROPPED_COUNTER.set(writer.sender());
    let sink: Arc<dyn InspectSink> = Arc::new(DiagnosticSink::new(writer.sender()));
    Some(Handle { sink, writer })
}

/// Live dropped-record count for the current session's dump, or `None`
/// when packet inspection was never turned on this run (`init` never
/// reached its `Some` branch, so [`DROPPED_COUNTER`] was never set). Used
/// by `crate::bundle::build_manifest`'s caller to report how incomplete an
/// in-progress (or just-finished) dump might be.
pub(crate) fn dropped_count() -> Option<u64> {
    DROPPED_COUNTER.get().map(dump::RecordSender::dropped_count)
}

#[derive(Debug, Clone)]
struct ServiceStat {
    count: u64,
    first_seen_ms: u64,
    last_method_id: u32,
    last_payload_len: usize,
    hex_prefix: String,
}

/// Per-`attr_id` aggregate (issue #69) — the fix for `attrs`'s old
/// `(uid, attr_id)` keying, which made "log a new fact" mean "log the same
/// ~60 facts once per entity". `uids` is capped at
/// [`MAX_TRACKED_UIDS_PER_ATTR`]; `uids_saturated` says whether the true
/// distinct-uid count ran past that cap.
#[derive(Debug, Clone)]
struct AttrStat {
    count: u64,
    uids: HashSet<i64>,
    uids_saturated: bool,
    sample_uid: i64,
    sample_raw_hex: String,
}

/// The real `InspectSink`: forwards every observation to the dump-writer
/// channel, and keeps an in-memory tally of unrecognized service ids and
/// unknown attr ids for the log-on-first-sight-plus-shutdown-summary
/// behavior described in the module doc comment.
struct DiagnosticSink {
    tx: dump::RecordSender,
    services: Mutex<HashMap<u64, ServiceStat>>,
    attrs: Mutex<HashMap<i32, AttrStat>>,
}

impl DiagnosticSink {
    fn new(tx: dump::RecordSender) -> Self {
        Self {
            tx,
            services: Mutex::new(HashMap::new()),
            attrs: Mutex::new(HashMap::new()),
        }
    }

    /// Records one observation of `attr_id` on `uid`, updating the per-attr
    /// aggregate. Returns `true` the first time this `attr_id` is seen at
    /// all — regardless of `uid` — which is exactly the "new unknown
    /// attr_id" discovery `on_attr` logs; `false` on every later
    /// observation. Split out as its own method (rather than inlined in
    /// `on_attr`) so the discovery decision is directly unit-testable.
    fn record_attr(&self, uid: i64, attr_id: i32, raw: &[u8]) -> bool {
        let mut attrs = self.attrs.lock().unwrap();
        let (stat, is_new) = match attrs.entry(attr_id) {
            std::collections::hash_map::Entry::Occupied(entry) => (entry.into_mut(), false),
            std::collections::hash_map::Entry::Vacant(entry) => (
                entry.insert(AttrStat {
                    count: 0,
                    uids: HashSet::new(),
                    uids_saturated: false,
                    sample_uid: uid,
                    sample_raw_hex: hex_prefix(raw),
                }),
                true,
            ),
        };
        stat.count += 1;
        if stat.uids.len() < MAX_TRACKED_UIDS_PER_ATTR {
            stat.uids.insert(uid);
        } else if !stat.uids.contains(&uid) {
            stat.uids_saturated = true;
        }
        is_new
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
        // One line per distinct attr_id (issue #69), not per (uid, attr_id)
        // pair — `attrs` is keyed by `attr_id` alone, so this is already
        // structurally guaranteed rather than something this loop has to
        // enforce.
        for (attr_id, stat) in self.attrs.lock().unwrap().iter() {
            let uids = stat.uids.len();
            if stat.uids_saturated {
                log::info!(
                    "packet-inspect summary: unknown attr_id=0x{attr_id:x} count={} uids={uids}+ (capped at {MAX_TRACKED_UIDS_PER_ATTR}) sample_uid={} sample_raw_hex={}",
                    stat.count,
                    stat.sample_uid,
                    stat.sample_raw_hex,
                );
            } else {
                log::info!(
                    "packet-inspect summary: unknown attr_id=0x{attr_id:x} count={} uids={uids} sample_uid={} sample_raw_hex={}",
                    stat.count,
                    stat.sample_uid,
                    stat.sample_raw_hex,
                );
            }
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
    fn on_notify(
        &self,
        service_uuid: u64,
        method_id: u32,
        payload: &[u8],
        payload_decoded: bool,
        now_ms: u64,
    ) {
        // Every service uuid, recognized or not — and every payload,
        // decompressed or not (`payload_decoded` tells a reader which) — feeds
        // the raw dump; it's what makes slice B's offline replay possible.
        self.tx.send(dump::Record {
            ts_ms: now_ms,
            service_uuid,
            method_id,
            payload: payload.to_vec(),
            payload_decoded,
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
                "packet-inspect: new unrecognized service_uuid=0x{service_uuid:016x} method_id=0x{method_id:08x} payload_len={} payload_decoded={payload_decoded} first_seen_ms={now_ms} hex_prefix={}",
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
        // Logged once per distinct attr_id, not once per (uid, attr_id) —
        // see the module doc comment and issue #69.
        if self.record_attr(uid, attr_id, raw) {
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
    fn enabled_is_true_for_1_true_on_or_yes_case_insensitive() {
        assert!(enabled_from(Some("1")));
        assert!(enabled_from(Some("true")));
        assert!(enabled_from(Some("TRUE")));
        assert!(enabled_from(Some("on")));
        assert!(enabled_from(Some("ON")));
        assert!(enabled_from(Some("yes")));
        assert!(enabled_from(Some("YES")));
    }

    #[test]
    fn enabled_is_false_for_0_or_any_other_unrecognized_value() {
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("false")));
        assert!(!enabled_from(Some("off")));
        assert!(!enabled_from(Some("2")));
        assert!(!enabled_from(Some("enabled")));
    }

    #[test]
    fn enabled_is_false_for_an_empty_value_since_only_the_named_tokens_opt_in() {
        assert!(!enabled_from(Some("")));
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
        assert!(path.starts_with("/appdata/ShinraMeter-BPSR/inspect"));
        assert!(path.to_string_lossy().ends_with(".jsonl"));
    }

    #[test]
    fn dump_path_falls_back_to_working_directory_when_neither_is_set() {
        let path = dump_path_from(None, None);
        assert!(
            path.to_string_lossy()
                .starts_with("ShinraMeter-BPSR-inspect-dump-")
        );
    }

    /// Issue #322: the filename is keyed by session id (`<pid>-<unix start
    /// seconds>`, `crate::logging::session_id`), not a bare pid — a bare
    /// pid gets reused across runs and can't tell two sessions' dumps
    /// apart.
    #[test]
    fn dump_path_is_keyed_by_the_process_wide_session_id_not_a_bare_pid() {
        let path = dump_path_from(None, Some("/appdata"));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("dump-{}.jsonl", crate::logging::session_id()));
    }

    // -- DiagnosticSink ---------------------------------------------------

    fn new_sink() -> (DiagnosticSink, crossbeam_channel::Receiver<dump::Record>) {
        let (tx, rx) = crossbeam_channel::bounded(16);
        (DiagnosticSink::new(dump::RecordSender::new(tx)), rx)
    }

    #[test]
    fn on_notify_forwards_every_service_to_the_dump_channel_in_order() {
        let (sink, rx) = new_sink();
        sink.on_notify(bpsr_protocol::frame::SERVICE_UUID, 1, b"a", true, 10);
        sink.on_notify(0xDEAD, 2, b"bb", true, 20);

        assert_eq!(
            rx.try_recv().unwrap(),
            dump::Record {
                ts_ms: 10,
                service_uuid: bpsr_protocol::frame::SERVICE_UUID,
                method_id: 1,
                payload: b"a".to_vec(),
                payload_decoded: true,
            }
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            dump::Record {
                ts_ms: 20,
                service_uuid: 0xDEAD,
                method_id: 2,
                payload: b"bb".to_vec(),
                payload_decoded: true,
            }
        );
    }

    /// A fragment whose payload wouldn't decompress still reaches the dump —
    /// as its raw bytes, flagged so a reader never feeds them to the decoder.
    #[test]
    fn on_notify_dumps_an_undecodable_payload_flagged_as_such() {
        let (sink, rx) = new_sink();
        sink.on_notify(0xDEAD, 3, b"raw-compressed", false, 30);

        assert_eq!(
            rx.try_recv().unwrap(),
            dump::Record {
                ts_ms: 30,
                service_uuid: 0xDEAD,
                method_id: 3,
                payload: b"raw-compressed".to_vec(),
                payload_decoded: false,
            }
        );
    }

    #[test]
    fn unrecognized_service_is_aggregated_with_a_count_and_first_seen_timestamp() {
        let (sink, _rx) = new_sink();
        sink.on_notify(0xDEAD, 1, b"a", true, 10);
        sink.on_notify(0xDEAD, 2, b"bb", true, 20);

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
        sink.on_notify(bpsr_protocol::frame::SERVICE_UUID, 1, b"a", true, 10);
        assert!(sink.services.lock().unwrap().is_empty());
    }

    /// Was `unknown_attr_is_counted_per_uid_and_attr_id`, asserting a count
    /// per `(uid, attr_id)` pair — the exact cardinality bug issue #69 fixes.
    /// Updated to assert the new per-`attr_id` aggregate: one entry keyed by
    /// `attr_id` alone, with `count` summed across uids and `uids` holding
    /// the distinct uids seen.
    #[test]
    fn unknown_attr_is_counted_per_attr_id_across_uids() {
        let (sink, _rx) = new_sink();
        sink.on_attr(5, 0x99, &[1, 2], false);
        sink.on_attr(5, 0x99, &[3], false);
        sink.on_attr(6, 0x99, &[], false);

        let attrs = sink.attrs.lock().unwrap();
        assert_eq!(attrs.len(), 1);
        let stat = attrs.get(&0x99).expect("attr 0x99 should be tracked");
        assert_eq!(stat.count, 3);
        assert_eq!(stat.uids, std::collections::HashSet::from([5, 6]));
    }

    /// The interesting discovery is a previously-unseen `attr_id`, not a
    /// previously-unseen `(uid, attr_id)` pair (issue #69) — `record_attr`
    /// (what `on_attr` logs on) must return `true` only for the very first
    /// sighting of a given `attr_id`, even when many different uids follow.
    #[test]
    fn record_attr_reports_new_discovery_once_per_attr_id_regardless_of_uid() {
        let (sink, _rx) = new_sink();
        assert!(sink.record_attr(5, 0x99, &[1, 2]));
        assert!(!sink.record_attr(6, 0x99, &[3]));
        assert!(!sink.record_attr(5, 0x99, &[4]));
        assert!(sink.record_attr(5, 0x42, &[5]));

        let attrs = sink.attrs.lock().unwrap();
        let stat = attrs.get(&0x99).unwrap();
        assert_eq!(stat.count, 3);
        assert_eq!(stat.uids.len(), 2);
        assert_eq!(attrs.get(&0x42).unwrap().count, 1);
    }

    /// `log_summary` iterates `attrs`, which is keyed by `attr_id` alone —
    /// this proves that map holds one entry per distinct attr id no matter
    /// how many uids it was observed on, i.e. `log_summary` emits one line
    /// per attr id rather than per (uid, attr_id) pair (issue #69).
    #[test]
    fn attrs_map_has_one_entry_per_attr_id_not_per_uid_attr_pair() {
        let (sink, _rx) = new_sink();
        for uid in 0..5 {
            sink.on_attr(uid, 0x99, &[uid as u8], false);
        }
        sink.on_attr(0, 0x42, &[], false);

        let attrs = sink.attrs.lock().unwrap();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs.get(&0x99).unwrap().count, 5);
        assert_eq!(attrs.get(&0x42).unwrap().count, 1);
    }

    /// Distinct-uid tracking is capped (`MAX_TRACKED_UIDS_PER_ATTR`) so an
    /// attr id seen on thousands of entities can't itself become an
    /// unbounded-memory version of the flood this fix removes; saturation
    /// is recorded rather than silently under-reporting.
    #[test]
    fn distinct_uid_tracking_is_capped_and_marks_saturation() {
        let (sink, _rx) = new_sink();
        for uid in 0..(MAX_TRACKED_UIDS_PER_ATTR as i64 + 3) {
            sink.record_attr(uid, 0x99, &[]);
        }

        let attrs = sink.attrs.lock().unwrap();
        let stat = attrs.get(&0x99).unwrap();
        assert_eq!(stat.uids.len(), MAX_TRACKED_UIDS_PER_ATTR);
        assert!(stat.uids_saturated);
        assert_eq!(stat.count, MAX_TRACKED_UIDS_PER_ATTR as u64 + 3);
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
