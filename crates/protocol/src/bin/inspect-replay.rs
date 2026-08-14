//! Offline replay/inspect binary (issue #25 slice B, item 4: "Offline
//! replay/inspect binary. A small `bin` that reads a dump, re-runs the
//! decoder, and prints histograms of service ids / method ids / attr ids
//! seen.").
//!
//! Reads the JSONL dump format written by `crates/app/src/dump.rs` (see that
//! module's doc comment for the authoritative on-disk shape) and rebuilds,
//! offline, exactly the histograms a live session would have produced:
//! service ids, method ids, and unknown attr ids, each with a count and
//! first/last-seen timestamp, and each marked as one we currently decode or
//! not. `--since`/`--until` narrow the replay to a millisecond window so a
//! dump can be diffed around a noted in-game event (see
//! `docs/packet-inspection.md`).
//!
//! ## Why this lives here (`crates/protocol/src/bin/`), not the app crate
//!
//! CI runs `cargo test --workspace --exclude shinra-bpsr` — the app crate
//! (`shinra-bpsr`) is excluded because `eframe` drags in GUI deps that don't
//! build/test cleanly in a headless CI environment. This binary must stay
//! host-runnable and host-testable there, and it only needs
//! `bpsr_protocol::{decode, frame, inspect, pb}`, all of which already live
//! in this crate — so a `src/bin/` binary here is the smallest way to get a
//! runnable, testable binary without a new workspace crate or any
//! app-crate dependency.
//!
//! ## How replay reconstructs the histograms
//!
//! The dump's `InspectSink::on_notify` hook (see `crates/protocol/src/inspect.rs`)
//! only fires from `frame.rs` during live capture — a dump record already
//! *is* one of those calls, captured to disk, so replaying it means driving
//! `decode::decode_notify` directly with a synthetic `Notify { method_id,
//! payload }` built from each record, rather than re-parsing outer frames.
//! Service-id and method-id histograms come straight from the dump records
//! themselves (every record already carries both); the unknown-attr-id
//! histogram comes from a local `InspectSink` that `decode_notify`'s attr
//! walk reports into exactly as it would during a live run — known attr ids
//! (`attrs::attr_id`) are deliberately never reported to a sink at all (see
//! `attrs::player_info_from_attrs`), so this histogram is unknown ids only,
//! by construction — which is the entire point per the issue.

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bpsr_protocol::decode::{decode_notify, opcode};
use bpsr_protocol::frame::{Notify, SERVICE_UUID};
use bpsr_protocol::inspect::InspectSink;
use serde::Deserialize;

/// One dump record after parsing, decoupled from the on-disk hex-string
/// encoding — see `crates/app/src/dump.rs` for the JSON shape this comes
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DumpRecord {
    ts_ms: u64,
    service_uuid: u64,
    method_id: u32,
    payload: Vec<u8>,
}

/// The on-disk JSON shape, straight off the wire — see `crates/app/src/dump.rs`.
#[derive(Deserialize)]
struct RawLine {
    ts_ms: u64,
    service_uuid: String,
    method_id: String,
    payload_hex: String,
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn parse_hex_u32(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Parses one JSONL line of the dump format into a `DumpRecord`. `Err`
/// (never a panic) on malformed JSON, a non-`0x`-prefixed hex field, or an
/// odd-length `payload_hex` — the caller skips the line and keeps going
/// rather than aborting the whole replay on one bad line.
fn parse_record(line: &str) -> Result<DumpRecord, String> {
    let raw: RawLine = serde_json::from_str(line).map_err(|err| err.to_string())?;
    let service_uuid = parse_hex_u64(&raw.service_uuid)
        .ok_or_else(|| format!("bad service_uuid: {}", raw.service_uuid))?;
    let method_id =
        parse_hex_u32(&raw.method_id).ok_or_else(|| format!("bad method_id: {}", raw.method_id))?;
    let payload = hex_decode(&raw.payload_hex)
        .ok_or_else(|| format!("bad payload_hex: {}", raw.payload_hex))?;
    Ok(DumpRecord {
        ts_ms: raw.ts_ms,
        service_uuid,
        method_id,
        payload,
    })
}

/// Count, first/last-seen timestamp, and whether an observed id is one we
/// currently decode.
#[derive(Debug, Clone, Default)]
struct IdStats {
    count: u64,
    first_ms: u64,
    last_ms: u64,
    known: bool,
}

impl IdStats {
    fn observe(&mut self, ts_ms: u64, known: bool) {
        if self.count == 0 {
            self.first_ms = ts_ms;
        }
        self.count += 1;
        self.last_ms = ts_ms;
        self.known = known;
    }
}

/// Same as `IdStats`, plus a sample so a human can eyeball what an unknown
/// attr's raw bytes look like (issue #12/#15's "list-shaped?" question).
#[derive(Debug, Clone, Default)]
struct AttrStats {
    count: u64,
    first_ms: u64,
    last_ms: u64,
    sample_uid: i64,
    sample_raw: Vec<u8>,
}

#[derive(Debug, Default)]
struct Histogram {
    services: BTreeMap<u64, IdStats>,
    /// Keyed by `(service_uuid, method_id)` — method ids are only meaningful
    /// within their own service, and a foreign service's method ids are
    /// exactly what issue #12 needs to see.
    methods: BTreeMap<(u64, u32), IdStats>,
    unknown_attrs: BTreeMap<i32, AttrStats>,
}

/// `true` for the four opcodes `decode::decode_notify` currently dispatches
/// on; everything else (recognized service or not) is "not decoded".
fn is_known_opcode(method_id: u32) -> bool {
    matches!(
        method_id,
        opcode::SYNC_NEAR_ENTITIES
            | opcode::SYNC_CONTAINER_DATA
            | opcode::SYNC_NEAR_DELTA_INFO
            | opcode::SYNC_TO_ME_DELTA_INFO
    )
}

/// Feeds `decode_notify`'s attr walk into `unknown_attrs`. `on_notify` is a
/// no-op here: that hook only ever fires from `frame.rs` during live
/// capture (parsing outer frames), never from `decode_notify` — replay
/// starts one layer downstream of it, already holding decompressed
/// records, so service/method histograms are built directly from the dump
/// records in `build_histogram` instead.
struct HistogramSink {
    histogram: Mutex<Histogram>,
    /// `on_unknown_attr` carries no timestamp of its own; `build_histogram`
    /// sets this to the current record's `ts_ms` before each `decode_notify`
    /// call so the attr walk's callback can still be timestamped.
    current_ts: AtomicU64,
}

impl HistogramSink {
    fn new() -> Self {
        Self {
            histogram: Mutex::new(Histogram::default()),
            current_ts: AtomicU64::new(0),
        }
    }

    fn set_current_ts(&self, ts_ms: u64) {
        self.current_ts.store(ts_ms, Ordering::Relaxed);
    }

    fn into_histogram(self) -> Histogram {
        self.histogram.into_inner().expect("mutex never poisoned")
    }
}

impl InspectSink for HistogramSink {
    fn on_notify(&self, _service_uuid: u64, _method_id: u32, _payload: &[u8], _now_ms: u64) {}

    fn on_unknown_attr(&self, uid: i64, attr_id: i32, raw: &[u8]) {
        let ts_ms = self.current_ts.load(Ordering::Relaxed);
        let mut h = self.histogram.lock().expect("mutex never poisoned");
        let entry = h.unknown_attrs.entry(attr_id).or_default();
        if entry.count == 0 {
            entry.first_ms = ts_ms;
        }
        entry.count += 1;
        entry.last_ms = ts_ms;
        entry.sample_uid = uid;
        entry.sample_raw = raw.to_vec();
    }
}

/// Replays `records` (already filtered to `[since, until]` if given) through
/// `decode_notify`, rebuilding the same histograms a live diagnostic run
/// would have produced.
fn build_histogram(
    records: impl Iterator<Item = DumpRecord>,
    since: Option<u64>,
    until: Option<u64>,
) -> Histogram {
    let sink = HistogramSink::new();
    for record in records {
        if since.is_some_and(|s| record.ts_ms < s) || until.is_some_and(|u| record.ts_ms > u) {
            continue;
        }
        let method_known = record.service_uuid == SERVICE_UUID && is_known_opcode(record.method_id);
        {
            let mut h = sink.histogram.lock().expect("mutex never poisoned");
            h.services
                .entry(record.service_uuid)
                .or_default()
                .observe(record.ts_ms, record.service_uuid == SERVICE_UUID);
            h.methods
                .entry((record.service_uuid, record.method_id))
                .or_default()
                .observe(record.ts_ms, method_known);
        }
        sink.set_current_ts(record.ts_ms);
        let notify = Notify {
            method_id: record.method_id,
            payload: record.payload,
        };
        let mut discarded_events = Vec::new();
        decode_notify(&notify, record.ts_ms, &mut discarded_events, Some(&sink));
    }
    sink.into_histogram()
}

/// Renders `h` as a human-readable summary: three sections (services,
/// methods, unknown attrs), unrecognized/undecoded entries called out
/// distinctly from known ones.
fn format_report(h: &Histogram) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    writeln!(out, "== Service IDs ==").unwrap();
    for (&uuid, stats) in &h.services {
        let label = if stats.known {
            "known (frame::SERVICE_UUID)"
        } else {
            "UNRECOGNIZED"
        };
        writeln!(
            out,
            "service=0x{uuid:016x}  count={} first_ms={} last_ms={} {label}",
            stats.count, stats.first_ms, stats.last_ms
        )
        .unwrap();
    }

    writeln!(out, "\n== Method IDs ==").unwrap();
    for (&(uuid, method_id), stats) in &h.methods {
        let label = if stats.known {
            "decoded"
        } else {
            "not decoded"
        };
        writeln!(
            out,
            "service=0x{uuid:016x} method=0x{method_id:08x}  count={} first_ms={} last_ms={} {label}",
            stats.count, stats.first_ms, stats.last_ms
        )
        .unwrap();
    }

    writeln!(
        out,
        "\n== Unknown attr IDs (no constant in attrs::attr_id) =="
    )
    .unwrap();
    if h.unknown_attrs.is_empty() {
        writeln!(out, "(none observed)").unwrap();
    }
    for (&attr_id, stats) in &h.unknown_attrs {
        let sample_hex: String = stats
            .sample_raw
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        writeln!(
            out,
            "attr_id=0x{attr_id:08x}  count={} first_ms={} last_ms={} sample_uid={} sample_raw_hex={sample_hex}",
            stats.count, stats.first_ms, stats.last_ms, stats.sample_uid
        )
        .unwrap();
    }

    out
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut path: Option<PathBuf> = None;
    let mut since: Option<u64> = None;
    let mut until: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => {
                i += 1;
                since = args.get(i).and_then(|s| s.parse().ok());
                if since.is_none() {
                    eprintln!("--since requires a millisecond integer");
                    return ExitCode::FAILURE;
                }
            }
            "--until" => {
                i += 1;
                until = args.get(i).and_then(|s| s.parse().ok());
                if until.is_none() {
                    eprintln!("--until requires a millisecond integer");
                    return ExitCode::FAILURE;
                }
            }
            other => path = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let Some(path) = path else {
        eprintln!("usage: inspect-replay <dump.jsonl> [--since MS] [--until MS]");
        return ExitCode::FAILURE;
    };
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("failed to open {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (lineno, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(err) => {
                eprintln!("line {}: read error: {err}", lineno + 1);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_record(&line) {
            Ok(r) => records.push(r),
            Err(err) => eprintln!("line {}: skipping malformed record: {err}", lineno + 1),
        }
    }
    let histogram = build_histogram(records.into_iter(), since, until);
    print!("{}", format_report(&histogram));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsr_protocol::pb;
    use prost::Message;

    // Matches the player-uuid convention already used by
    // `bpsr_protocol::decode`'s own tests: `(uid << 16) | 640` is a player
    // entity, not a guessed constant.
    const PLAYER_UUID: i64 = (7i64 << 16) | 640;

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn delta_notify_payload(attr_id: i32, raw: Vec<u8>) -> Vec<u8> {
        let attrs = pb::AttrCollection {
            uuid: PLAYER_UUID,
            attrs: vec![pb::Attr {
                id: attr_id,
                raw_data: raw,
            }],
        };
        let delta = pb::AoiSyncDelta {
            uuid: PLAYER_UUID,
            attrs: Some(attrs),
            skill_effects: None,
        };
        let msg = pb::SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        payload
    }

    fn known_attr_payload() -> Vec<u8> {
        delta_notify_payload(bpsr_protocol::attrs::attr_id::NAME, vec![0xFF, b'H', b'i'])
    }

    #[test]
    fn parse_record_parses_a_well_formed_dump_line() {
        let payload = delta_notify_payload(0x7777, vec![0x01]);
        let line = format!(
            r#"{{"ts_ms":100,"service_uuid":"0x0000000063335342","method_id":"0x0000002d","payload_hex":"{}"}}"#,
            to_hex(&payload)
        );

        let record = parse_record(&line).expect("well-formed line must parse");

        assert_eq!(record.ts_ms, 100);
        assert_eq!(record.service_uuid, SERVICE_UUID);
        assert_eq!(record.method_id, opcode::SYNC_NEAR_DELTA_INFO);
        assert_eq!(record.payload, payload);
    }

    #[test]
    fn parse_record_rejects_malformed_json() {
        assert!(parse_record("not json").is_err());
    }

    #[test]
    fn parse_record_rejects_odd_length_payload_hex() {
        let line = r#"{"ts_ms":1,"service_uuid":"0x0000000063335342","method_id":"0x00000001","payload_hex":"abc"}"#;
        assert!(parse_record(line).is_err());
    }

    #[test]
    fn build_histogram_classifies_services_methods_and_unknown_attrs() {
        let recognized_known_attr = DumpRecord {
            ts_ms: 100,
            service_uuid: SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload: known_attr_payload(),
        };
        let recognized_unknown_attr = DumpRecord {
            ts_ms: 150,
            service_uuid: SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload: delta_notify_payload(0x7777, vec![0x02]),
        };
        let other_service = SERVICE_UUID.wrapping_add(1);
        let unrecognized = DumpRecord {
            ts_ms: 200,
            service_uuid: other_service,
            method_id: 0x42,
            payload: b"hello".to_vec(),
        };

        let histogram = build_histogram(
            vec![recognized_known_attr, recognized_unknown_attr, unrecognized].into_iter(),
            None,
            None,
        );

        let svc = histogram
            .services
            .get(&SERVICE_UUID)
            .expect("recognized service present");
        assert_eq!(svc.count, 2);
        assert!(svc.known);
        assert_eq!(svc.first_ms, 100);
        assert_eq!(svc.last_ms, 150);

        let other = histogram
            .services
            .get(&other_service)
            .expect("unrecognized service present");
        assert_eq!(other.count, 1);
        assert!(!other.known);

        let method = histogram
            .methods
            .get(&(SERVICE_UUID, opcode::SYNC_NEAR_DELTA_INFO))
            .expect("known method present");
        assert_eq!(method.count, 2);
        assert!(method.known);

        let other_method = histogram
            .methods
            .get(&(other_service, 0x42))
            .expect("unrecognized method present");
        assert!(!other_method.known);

        assert!(
            !histogram
                .unknown_attrs
                .contains_key(&bpsr_protocol::attrs::attr_id::NAME)
        );
        let unknown = histogram
            .unknown_attrs
            .get(&0x7777)
            .expect("unknown attr id observed");
        assert_eq!(unknown.count, 1);
        assert_eq!(
            unknown.sample_uid,
            bpsr_protocol::event::uid_of(PLAYER_UUID)
        );
        assert_eq!(unknown.first_ms, 150);
    }

    #[test]
    fn build_histogram_respects_since_and_until_window() {
        let in_window = DumpRecord {
            ts_ms: 500,
            service_uuid: SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload: known_attr_payload(),
        };
        let before = DumpRecord {
            ts_ms: 100,
            ..in_window.clone()
        };
        let after = DumpRecord {
            ts_ms: 900,
            ..in_window.clone()
        };

        let histogram = build_histogram(
            vec![before, in_window, after].into_iter(),
            Some(300),
            Some(700),
        );

        let svc = histogram.services.get(&SERVICE_UUID).unwrap();
        assert_eq!(svc.count, 1);
        assert_eq!(svc.first_ms, 500);
    }

    #[test]
    fn format_report_calls_out_unrecognized_and_unknown_distinctly() {
        let other_service = SERVICE_UUID.wrapping_add(1);
        let records = vec![
            DumpRecord {
                ts_ms: 1,
                service_uuid: other_service,
                method_id: 0x42,
                payload: b"hello".to_vec(),
            },
            DumpRecord {
                ts_ms: 2,
                service_uuid: SERVICE_UUID,
                method_id: opcode::SYNC_NEAR_DELTA_INFO,
                payload: delta_notify_payload(0x7777, vec![0x02]),
            },
        ];
        let histogram = build_histogram(records.into_iter(), None, None);

        let report = format_report(&histogram);

        assert!(report.contains("UNRECOGNIZED"));
        assert!(report.contains("attr_id=0x00007777"));
    }
}
