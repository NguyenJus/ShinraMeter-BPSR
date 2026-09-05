//! Builds a committable, PII-scrubbed fixture from a `SHINRA_INSPECT=1` JSONL
//! dump (see `crates/protocol/src/dump_format.rs` for the on-disk shape).
//!
//! The actual scrubbing (decode -> rewrite identifying uids/names ->
//! re-encode through the partial `pb.rs` schema, dropping anything
//! unmodeled) lives in the library module `bpsr_protocol::sanitize` (issue
//! #346) — this binary is a thin wrapper around it that adds the two things
//! only a batch tool can do: read a whole dump window up front, and refuse
//! to write output unless both mandatory self-checks below pass. The live
//! dump writer (`crates/app/src/dump.rs`) uses the same library module
//! directly, record-at-a-time, with no self-check (it has no "whole
//! window" to compare against).
//!
//! ## Mandatory self-check
//!
//! This tool refuses to write output unless both of the following hold:
//!
//! 1. **Fingerprint equality**: replaying the original window's records and
//!    the sanitized records separately through `decode_notify -> Meter::apply
//!    -> Meter::snapshot` must produce identical fingerprints — duration,
//!    total damage/DPS, every row's damage/hits/crit%/lucky%/deaths/class/
//!    ability_score (compared uid-order-independent, sorted by damage), and
//!    the full `EncounterInfo` — for every fight-end snapshot plus the final
//!    snapshot.
//! 2. **No residual strings**: every field that can carry text in any of the
//!    five modeled message shapes — `Attr.raw_data` where `id == NAME`, and
//!    `CharBaseInfo.name` — must decode to the `PlayerNNNNN` placeholder
//!    pattern (or be empty). See [`check_no_residual_strings`]'s doc comment
//!    for why a schema-typed check catches everything a schema-agnostic byte
//!    walk would, without that approach's false positives on numeric attrs
//!    that happen to be valid UTF-8.
//!
//! A failure on either check aborts before any output is written, and the
//! CLI exits non-zero.

use std::path::PathBuf;
use std::process::ExitCode;

use bpsr_meter as meter;
use bpsr_protocol as proto;
use bpsr_protocol::decode::decode_notify;
use bpsr_protocol::dump_format::{self, DumpRecord};
use bpsr_protocol::frame::Notify;
use bpsr_protocol::sanitize;

// `map_class`/`map_event` used to be duplicated here field-for-field with
// `crates/app/src/pipeline.rs` (issue #146's finding 2); both now share
// `bpsr_protocol::map`, called directly at `drive`'s one call site below
// (this binary has no Imagine catalog, so it always passes `None`).
// `EntityKind` is now the one shared type between `bpsr-protocol` and
// `bpsr-meter` too (issue #371).

/// Drives the meter over `records` (assumed already time-ordered) and
/// returns every fight-end snapshot plus the snapshot as of the last record.
///
/// Defense in depth: `records` should never actually be empty here (callers
/// are expected to refuse before reaching this point — see `main`'s
/// `clean.is_empty()` check), but an empty slice is handled rather than
/// indexed into blindly, so a caller mistake produces an empty result
/// instead of a panic.
fn drive(records: &[DumpRecord]) -> (Vec<meter::Snapshot>, meter::Snapshot) {
    let mut m = meter::Meter::new();
    let Some(first) = records.first() else {
        return (Vec::new(), m.snapshot(0));
    };
    let mut clock = first.ts_ms;
    // One table for the whole replay, exactly as a live `Decoder` keeps one
    // for the whole session (issue #335) — entity identity is cross-packet
    // state, and a per-record table would resolve every bare `char_id`
    // through the fallback instead of the shadow map.
    let mut entities = proto::EntityTable::new();
    let mut state = meter::FightState::Idle;
    let mut ends = Vec::new();
    for r in records {
        while clock + 100 <= r.ts_ms {
            clock += 100;
            let st = m.tick(clock);
            if state == meter::FightState::Active && st == meter::FightState::Ended {
                ends.push(m.snapshot(clock));
            }
            state = st;
        }
        if !r.payload_decoded {
            continue;
        }
        let n = Notify {
            service_uuid: r.service_uuid,
            method_id: r.method_id,
            payload: r.payload.clone(),
        };
        let mut evs = Vec::new();
        decode_notify(&n, r.ts_ms, &mut evs, None, &mut entities);
        for ev in evs {
            m.apply(&proto::map::map_event(ev, r.ts_ms, None, None));
        }
        clock = clock.max(r.ts_ms);
        let st = m.fight_state(clock);
        if state == meter::FightState::Active && st == meter::FightState::Ended {
            ends.push(m.snapshot(clock));
        }
        state = st;
    }
    let last = records.last().unwrap().ts_ms;
    (ends, m.snapshot(last))
}

/// A stable, order-independent (rows sorted by damage) textual summary of a
/// snapshot, for the self-check's equality assertion.
fn fingerprint(s: &meter::Snapshot) -> String {
    let mut rows: Vec<String> = s
        .rows
        .iter()
        .map(|r| {
            format!(
                "dmg={} hits={} crit={:.2} lucky={:.2} deaths={} class={:?} ap={:?}",
                r.damage, r.hits, r.crit_pct, r.lucky_pct, r.deaths, r.class, r.ability_score
            )
        })
        .collect();
    rows.sort();
    format!(
        "dur={} total={} dps={:.3} enc={:?} rows=[{}]",
        s.duration_ms,
        s.total_damage,
        s.total_dps,
        s.encounter,
        rows.join(" | ")
    )
}

/// Self-check 1: identical fight-end and final-snapshot fingerprints between
/// `orig` and `clean`. Returns `Err` with a human-readable diagnosis on the
/// first mismatch (or count mismatch) rather than panicking, so `main` can
/// report and exit cleanly.
fn check_fingerprints(orig: &[DumpRecord], clean: &[DumpRecord]) -> Result<(), String> {
    let (ends_a, fin_a) = drive(orig);
    let (ends_b, fin_b) = drive(clean);
    if ends_a.len() != ends_b.len() {
        return Err(format!(
            "fight-end snapshot count differs: original={} sanitized={}",
            ends_a.len(),
            ends_b.len()
        ));
    }
    for (i, (a, b)) in ends_a.iter().zip(ends_b.iter()).enumerate() {
        let (fa, fb) = (fingerprint(a), fingerprint(b));
        if fa != fb {
            return Err(format!(
                "fight #{i} fingerprint mismatch:\n  original:  {fa}\n  sanitized: {fb}"
            ));
        }
    }
    let (fa, fb) = (fingerprint(&fin_a), fingerprint(&fin_b));
    if fa != fb {
        return Err(format!(
            "final snapshot fingerprint mismatch:\n  original:  {fa}\n  sanitized: {fb}"
        ));
    }
    Ok(())
}

// ---- Self-check 2: no residual strings ------------------------------------
//
// Deliberately schema-*typed*, not a blind byte walk. Why that's still a
// strong, non-tautological check rather than just re-trusting the
// sanitizer's own whitelist: `sanitize::sanitize` re-encodes through
// `pb.rs`'s prost-derived structs, and prost *silently drops any field it
// has no struct field for* on decode. That means a field this tool forgot
// to model cannot survive the round-trip at all, regardless of how the
// residual-string check is implemented -- the round-trip itself is what
// enforces "nothing unmodeled leaks". What a walk still needs to catch is a
// *modeled* text field this tool forgot to scrub. A generic schema-agnostic
// byte walk was tried first and produced a false positive: a 4-byte `HP`
// attr value (`attr_id::HP`, a plain numeric telemetry field, never text)
// happened to be valid UTF-8 containing an alphabetic code point. Walking
// the typed structs instead checks exactly the fields that can legitimately
// carry text -- `Attr.raw_data` when `id == NAME`, and `CharBaseInfo.name` --
// against the placeholder pattern, with no room for numeric coincidences.
fn read_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Decodes the `varint length + UTF-8 bytes` shape `sanitize::encode_name_attr`
/// writes. `None` on anything that doesn't match that exact shape.
fn decode_name_attr(raw: &[u8]) -> Option<String> {
    let mut i = 0usize;
    let len = read_varint(raw, &mut i)? as usize;
    let bytes = raw.get(i..i + len)?;
    if i + len != raw.len() {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

/// `PlayerNNNNN` where `NNNNN` is the remapped uid -- the only shape a
/// text-like leaf is allowed to have in sanitized output.
fn is_placeholder(s: &str) -> bool {
    s.strip_prefix("Player")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Checks every `NAME` attr inside `ac` (and nowhere else -- every other attr
/// id surviving `scrub_attrs`'s `KEEP_ATTRS` filter is documented numeric
/// telemetry, per `attrs.rs`) is a placeholder.
fn check_attrs(ac: &proto::pb::AttrCollection, ctx: &str) -> Result<(), String> {
    for a in &ac.attrs {
        if a.id != bpsr_protocol::attrs::attr_id::NAME {
            continue;
        }
        let name = decode_name_attr(&a.raw_data).ok_or_else(|| {
            format!(
                "{ctx}: NAME attr did not decode as a name (bug in this tool, not a data problem)"
            )
        })?;
        if !is_placeholder(&name) {
            return Err(format!(
                "{ctx}: NAME attr is not a placeholder (length {} bytes; content withheld -- treat as PII)",
                name.len()
            ));
        }
    }
    Ok(())
}

fn check_delta(d: &proto::pb::AoiSyncDelta, ctx: &str) -> Result<(), String> {
    if let Some(ac) = &d.attrs {
        check_attrs(ac, ctx)?;
    }
    Ok(())
}

fn check_char_base(name: &str, ctx: &str) -> Result<(), String> {
    if !name.is_empty() && !is_placeholder(name) {
        return Err(format!(
            "{ctx}: CharBaseInfo.name is not a placeholder (length {} bytes; content withheld -- treat as PII)",
            name.len()
        ));
    }
    Ok(())
}

/// Self-check 2: every text-capable field in every sanitized payload is
/// either empty or a `PlayerNNNNN` placeholder. Returns `Err` naming the
/// offending record's method id and ts -- deliberately never the string
/// content itself, since a real failure here means real PII.
fn check_no_residual_strings(clean: &[DumpRecord]) -> Result<(), String> {
    use bpsr_protocol::decode::opcode;
    use prost::Message as _;

    for r in clean {
        let ctx = format!("method_id=0x{:08x} ts_ms={}", r.method_id, r.ts_ms);
        match r.method_id {
            opcode::SYNC_NEAR_ENTITIES => {
                let m = proto::pb::SyncNearEntities::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
                for e in &m.appear {
                    if let Some(ac) = &e.attrs {
                        check_attrs(ac, &ctx)?;
                    }
                }
            }
            opcode::SYNC_NEAR_DELTA_INFO => {
                let m = proto::pb::SyncNearDeltaInfo::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
                for d in &m.delta_infos {
                    check_delta(d, &ctx)?;
                }
            }
            opcode::SYNC_TO_ME_DELTA_INFO => {
                let m = proto::pb::SyncToMeDeltaInfo::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
                if let Some(di) = &m.delta_info
                    && let Some(bd) = &di.base_delta
                {
                    check_delta(bd, &ctx)?;
                }
            }
            opcode::SYNC_CONTAINER_DATA => {
                let m = proto::pb::SyncContainerData::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
                if let Some(v) = &m.v_data
                    && let Some(cb) = &v.char_base
                {
                    check_char_base(&cb.name, &ctx)?;
                }
            }
            opcode::ENTER_SCENE => {
                let m = proto::pb::EnterScene::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
                if let Some(info) = &m.info
                    && let Some(ac) = &info.attrs
                {
                    check_attrs(ac, &ctx)?;
                }
            }
            // Issue #139 verified all six real captures of `DungeonSyncData`
            // and all 392 real `SyncDungeonDirtyData` payloads carry no
            // player-identifying strings (dungeon flow/timer/target state
            // only) -- there's nothing here for `check_attrs`/
            // `check_char_base` to walk, just a re-decode to confirm the
            // sanitized bytes are still a well-formed instance of the
            // modeled message.
            opcode::SYNC_DUNGEON_DATA => {
                proto::pb::DungeonSyncData::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
            }
            opcode::SYNC_DUNGEON_DIRTY_DATA => {
                proto::pb::SyncDungeonDirtyData::decode(r.payload.as_slice())
                    .map_err(|e| format!("{ctx}: re-decode failed: {e}"))?;
            }
            other => {
                return Err(format!(
                    "{ctx}: unexpected unmodeled opcode 0x{other:08x} in sanitized output                      (should have been dropped by `sanitize::is_modeled` already)"
                ));
            }
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "usage: sanitize-dump <input.jsonl> --out <output.jsonl[.zst]> [--since MS] [--until MS]"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut input: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut since: Option<u64> = None;
    let mut until: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            "--since" => {
                i += 1;
                since = args.get(i).and_then(|s| s.parse().ok());
            }
            "--until" => {
                i += 1;
                until = args.get(i).and_then(|s| s.parse().ok());
            }
            other => input = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let (Some(input), Some(out)) = (input, out) else {
        print_usage();
        return ExitCode::FAILURE;
    };

    let all = match dump_format::load_dump(&input) {
        Ok(records) => records,
        Err(err) => {
            eprintln!("failed to read {}: {err}", input.display());
            return ExitCode::FAILURE;
        }
    };
    let since = since.unwrap_or(0);
    let until = until.unwrap_or(u64::MAX);
    let orig: Vec<DumpRecord> = all
        .into_iter()
        .filter(|r| r.ts_ms >= since && r.ts_ms <= until && sanitize::is_modeled(r.method_id))
        .collect();
    if orig.is_empty() {
        eprintln!("no modeled records found in window [{since}, {until}]");
        return ExitCode::FAILURE;
    }
    eprintln!("window records (modeled opcodes only): {}", orig.len());

    let mut remap = sanitize::Remap::new();
    let mut clean: Vec<DumpRecord> = Vec::with_capacity(orig.len());
    let mut failed = 0u64;
    for r in &orig {
        // A record the original capture couldn't decompress carries raw
        // compressed bytes, not protobuf — sanitize() would just fail to
        // parse it. Skip it like `drive` and the replay-dump test do (see
        // `DumpRecord::payload_decoded`'s doc comment), rather than letting
        // it silently pad the "failed to decode" count below with fragments
        // that were never modeled data in the first place.
        if !r.payload_decoded {
            continue;
        }
        match sanitize::sanitize(r.method_id, &r.payload, &mut remap) {
            Some(payload) => clean.push(DumpRecord {
                ts_ms: r.ts_ms,
                service_uuid: r.service_uuid,
                method_id: r.method_id,
                payload,
                payload_decoded: true,
            }),
            None => failed += 1,
        }
    }
    if failed > 0 {
        eprintln!(
            "warning: {failed} of {} modeled record(s) failed to decode/re-encode and were dropped",
            orig.len()
        );
    }
    eprintln!("distinct uids remapped: {}", remap.uids.len());

    if clean.is_empty() {
        eprintln!(
            "SELF-CHECK FAILED (fingerprint equality): no records survived sanitization ({failed} of {} modeled record(s) failed to decode/re-encode); nothing to compare.",
            orig.len()
        );
        eprintln!("refusing to write output.");
        return ExitCode::FAILURE;
    }

    eprintln!("running self-check 1/2: fingerprint equality...");
    if let Err(err) = check_fingerprints(&orig, &clean) {
        eprintln!("SELF-CHECK FAILED (fingerprint equality):\n{err}");
        eprintln!("refusing to write output.");
        return ExitCode::FAILURE;
    }
    eprintln!("  PASS");

    eprintln!("running self-check 2/2: no residual strings...");
    if let Err(err) = check_no_residual_strings(&clean) {
        eprintln!("SELF-CHECK FAILED (no residual strings):\n{err}");
        eprintln!("refusing to write output.");
        return ExitCode::FAILURE;
    }
    eprintln!("  PASS");

    // Rebase timestamps to start near zero, purely to keep the fixture
    // small and free of any wall-clock information about when it was
    // captured; the meter only ever consumes deltas.
    let base = clean[0].ts_ms;
    let mut jsonl = String::new();
    for r in &clean {
        jsonl.push_str(&dump_format::format_line(&DumpRecord {
            ts_ms: r.ts_ms - base,
            service_uuid: r.service_uuid,
            method_id: r.method_id,
            payload: r.payload.clone(),
            payload_decoded: true,
        }));
    }

    let raw_bytes = jsonl.len();
    let write_result = if out.extension().is_some_and(|e| e == "zst") {
        match zstd::stream::encode_all(jsonl.as_bytes(), 19) {
            Ok(compressed) => {
                eprintln!(
                    "raw jsonl: {raw_bytes} bytes; zstd-compressed: {} bytes ({:.1}%)",
                    compressed.len(),
                    100.0 * compressed.len() as f64 / raw_bytes as f64
                );
                std::fs::write(&out, compressed)
            }
            Err(err) => {
                eprintln!("zstd compression failed: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        eprintln!("raw jsonl: {raw_bytes} bytes (uncompressed output)");
        std::fs::write(&out, jsonl)
    };
    if let Err(err) = write_result {
        eprintln!("failed to write {}: {err}", out.display());
        return ExitCode::FAILURE;
    }
    eprintln!("wrote {}", out.display());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsr_protocol::decode::opcode;
    use prost::Message;

    #[test]
    fn is_placeholder_accepts_the_stable_name_shape() {
        assert!(is_placeholder("Player100000"));
        assert!(is_placeholder("Player0"));
    }

    #[test]
    fn is_placeholder_rejects_anything_else() {
        assert!(!is_placeholder("Player"));
        assert!(!is_placeholder("PlayerAbc"));
        assert!(!is_placeholder("SomeRealName"));
        assert!(!is_placeholder("player123"));
    }

    #[test]
    fn decode_name_attr_roundtrips_with_the_sanitizer_s_own_encoding() {
        // `sanitize::encode_name_attr` is private, so this exercises the
        // roundtrip through a real sanitized payload instead — see
        // `sync_container_data_uses_the_bare_uid_rule_for_char_id` in the
        // `sanitize` module for the analogous `CharBaseInfo.name` path.
        let mut r = sanitize::Remap::new();
        let attrs = proto::pb::AttrCollection {
            uuid: (7i64 << 16) | 640,
            attrs: vec![proto::pb::Attr {
                id: bpsr_protocol::attrs::attr_id::NAME,
                raw_data: b"placeholder-shaped input, contents ignored".to_vec(),
            }],
        };
        let delta = proto::pb::AoiSyncDelta {
            uuid: (7i64 << 16) | 640,
            attrs: Some(attrs),
            skill_effects: None,
            buff_effect: None,
        };
        let payload = proto::pb::SyncNearDeltaInfo {
            delta_infos: vec![delta],
        }
        .encode_to_vec();
        let sanitized = sanitize::sanitize(opcode::SYNC_NEAR_DELTA_INFO, &payload, &mut r).unwrap();
        let decoded = proto::pb::SyncNearDeltaInfo::decode(sanitized.as_slice()).unwrap();
        let raw = &decoded.delta_infos[0].attrs.as_ref().unwrap().attrs[0].raw_data;
        let name = decode_name_attr(raw).expect("must decode back to a name string");
        assert!(is_placeholder(&name));
    }

    #[test]
    fn check_no_residual_strings_passes_a_freshly_sanitized_record() {
        let mut r = sanitize::Remap::new();
        let payload = proto::pb::SyncNearDeltaInfo {
            delta_infos: vec![proto::pb::AoiSyncDelta {
                uuid: (7i64 << 16) | 640,
                attrs: Some(proto::pb::AttrCollection {
                    uuid: (7i64 << 16) | 640,
                    attrs: vec![proto::pb::Attr {
                        id: bpsr_protocol::attrs::attr_id::NAME,
                        raw_data: b"whatever, gets overwritten".to_vec(),
                    }],
                }),
                skill_effects: None,
                buff_effect: None,
            }],
        }
        .encode_to_vec();
        let sanitized = sanitize::sanitize(opcode::SYNC_NEAR_DELTA_INFO, &payload, &mut r).unwrap();
        let record = DumpRecord {
            ts_ms: 1,
            service_uuid: proto::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload: sanitized,
            payload_decoded: true,
        };
        assert!(check_no_residual_strings(&[record]).is_ok());
    }

    #[test]
    fn check_no_residual_strings_accepts_sanitized_dungeon_records() {
        // Issue #139 verified neither dungeon opcode carries player strings
        // -- `check_no_residual_strings` just needs to re-decode them
        // successfully rather than reject them as unmodeled.
        let mut r = sanitize::Remap::new();

        let dungeon_sync_payload = proto::pb::DungeonSyncData {
            scene_uuid: 42,
            flow_info: Some(proto::pb::DungeonFlowInfo { state: 1 }),
            target: None,
            dungeon_var: None,
        }
        .encode_to_vec();
        let dungeon_sync_sanitized =
            sanitize::sanitize(opcode::SYNC_DUNGEON_DATA, &dungeon_sync_payload, &mut r).unwrap();

        let dirty_data_payload = proto::pb::SyncDungeonDirtyData {
            v_data: Some(proto::pb::BufferStream {
                stream_type: 0,
                buffer: vec![],
            }),
        }
        .encode_to_vec();
        let dirty_data_sanitized =
            sanitize::sanitize(opcode::SYNC_DUNGEON_DIRTY_DATA, &dirty_data_payload, &mut r)
                .unwrap();

        let records = [
            DumpRecord {
                ts_ms: 1,
                service_uuid: proto::frame::SERVICE_UUID,
                method_id: opcode::SYNC_DUNGEON_DATA,
                payload: dungeon_sync_sanitized,
                payload_decoded: true,
            },
            DumpRecord {
                ts_ms: 2,
                service_uuid: proto::frame::SERVICE_UUID,
                method_id: opcode::SYNC_DUNGEON_DIRTY_DATA,
                payload: dirty_data_sanitized,
                payload_decoded: true,
            },
        ];
        assert!(check_no_residual_strings(&records).is_ok());
    }

    #[test]
    fn drive_handles_an_empty_slice_without_panicking() {
        // Defense in depth for the same case `check_fingerprints_...` below
        // exercises at the `main`-refusal level: `drive` used to index
        // `records[0]` unconditionally.
        let (ends, fin) = drive(&[]);
        assert!(ends.is_empty());
        assert_eq!(fin.duration_ms, 0);
        assert_eq!(fin.total_damage, 0);
    }

    #[test]
    fn check_fingerprints_reports_an_error_instead_of_panicking_when_clean_is_empty() {
        // Models the case `main` must now refuse before ever reaching this
        // function: every record in `orig` failed to sanitize, so `clean`
        // is empty. Before this fix, `check_fingerprints` -> `drive(clean)`
        // indexed `records[0]` on the empty slice and panicked instead of
        // returning the tool's normal "SELF-CHECK FAILED" diagnosis.
        let payload = proto::pb::SyncNearDeltaInfo {
            delta_infos: vec![proto::pb::AoiSyncDelta {
                uuid: (2i64 << 16) | 64, // a monster, the damage's target
                attrs: None,
                skill_effects: Some(proto::pb::SkillEffect {
                    damages: vec![proto::pb::SyncDamageInfo {
                        is_miss: false,
                        r#type: proto::pb::EDamageType::Normal as i32,
                        type_flag: 0,
                        value: 1000,
                        lucky_value: 0,
                        hp_lessen_value: 0,
                        attacker_uuid: (1i64 << 16) | 640, // a player, the attacker
                        owner_id: 1,
                        is_dead: false,
                        top_summoner_id: 0,
                    }],
                }),
                buff_effect: None,
            }],
        }
        .encode_to_vec();
        let orig = vec![DumpRecord {
            ts_ms: 1_000,
            service_uuid: proto::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
            payload_decoded: true,
        }];

        let err = check_fingerprints(&orig, &[])
            .expect_err("an empty clean must not fingerprint-match a non-empty orig");
        assert!(
            err.contains("mismatch"),
            "expected a diagnosable mismatch message, got: {err}"
        );
    }
}
