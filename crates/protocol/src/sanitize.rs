//! PII-scrubbing core shared by the offline `sanitize-dump` batch tool
//! (`src/bin/sanitize-dump.rs`) and the live dump writer
//! (`crates/app/src/dump.rs`, issue #346) — moved here so the writer can
//! sanitize records as they're captured instead of requiring a separate
//! post-process step before a dump is safe to share.
//!
//! ## Safety property: whitelist by re-encode
//!
//! Each record's payload is decoded with `pb.rs` — a hand-written *partial*
//! protobuf schema — and re-encoded. Any field `pb.rs` doesn't model is
//! silently dropped by this round-trip; any opcode `pb.rs` doesn't model at
//! all is dropped from the output entirely (see [`is_modeled`]). That drop
//! is the safety property: nothing exits [`sanitize`] that isn't explicitly
//! accounted for below. This is deliberately not a surgical byte-patch that
//! preserves unknown fields — that would leak the 36-char session GUIDs
//! riding `EnterScene`'s unmodeled fields 1.7/1.8 and opcode `0x04`'s field
//! 1.2.
//!
//! Player-identifying uids are consistently remapped (see [`Remap`]) and
//! names are replaced with stable `PlayerNNNNN` placeholders. Two uid
//! shapes exist in the wire format and must not be confused: most uids are
//! *packed* `uuid = (uid << 16) | kind_bits`, but `CharSerialize.char_id`
//! and `CharBaseInfo.char_id` are *bare* uids — applying the packed-uuid
//! rule to them silently corrupts the local player's class/ability_score
//! while leaving every damage number byte-identical.
//!
//! [`sanitize-dump`](../../bin/sanitize-dump.rs) additionally runs a
//! mandatory self-check (fingerprint equality through `bpsr_meter::Meter`,
//! plus a no-residual-strings scan) before it will write output — that
//! check is batch-only (it needs the whole window of records up front) and
//! stays in the binary. [`Sanitizer`], used by the live writer, has no such
//! check: it relies solely on the whitelist-by-re-encode property above,
//! record by record, as they're captured.

use std::collections::BTreeMap;

use prost::Message;

use crate::decode::opcode;
use crate::dump_format::DumpRecord;
use crate::pb;

/// A stable old-uid -> new-uid remap, assigning small sequential ids
/// (starting at 100_000, clear of any real uid range) in first-seen order
/// so the same player gets the same placeholder everywhere.
pub struct Remap {
    pub uids: BTreeMap<i64, i64>,
    next: i64,
}

impl Default for Remap {
    fn default() -> Self {
        Self::new()
    }
}

impl Remap {
    pub fn new() -> Self {
        Self {
            uids: BTreeMap::new(),
            next: 100_000,
        }
    }

    fn uid(&mut self, old: i64) -> i64 {
        if old == 0 {
            return 0;
        }
        let n = self.next;
        let v = *self.uids.entry(old).or_insert(n);
        if v == n {
            self.next += 1;
        }
        v
    }

    /// Remaps a *packed* `uuid = (uid << 16) | kind_bits`, preserving the
    /// low 16 bits (which encode entity kind, not identity).
    fn uuid(&mut self, old: i64) -> i64 {
        if old == 0 {
            return 0;
        }
        let uid = old >> 16;
        let low = old & 0xFFFF;
        (self.uid(uid) << 16) | low
    }

    fn name_for(&mut self, uuid: i64) -> String {
        format!("Player{}", self.uuid(uuid) >> 16)
    }
}

fn encode_name_attr(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    prost::encoding::encode_varint(name.len() as u64, &mut out);
    out.extend_from_slice(name.as_bytes());
    out
}

/// Attr ids anything downstream of `decode_notify` actually reads — every
/// other attr id is dropped by `scrub_attrs`, unconditionally. This is what
/// keeps the "no residual strings" self-check trivially true: none of these
/// carry text except `NAME`, which is always overwritten.
const KEEP_ATTRS: [i32; 10] = [
    0x01,   // NAME
    0x0A,   // MONSTER_ID
    0x2C2E, // HP
    0x2C38, // MAX_HP
    0xDC,   // PROFESSION_ID
    0x272E, // FIGHT_POINT
    0x2756, // SEASON_LEVEL
    0x2CB0, // SEASON_STRENGTH
    0x74,   // SKILL_LEVEL_ID_LIST
    0x155,  // SCENE_BASIC_ID
];

pub fn scrub_attrs(ac: &mut pb::AttrCollection, r: &mut Remap, owner_uuid: i64) {
    ac.uuid = r.uuid(ac.uuid);
    ac.attrs.retain(|a| KEEP_ATTRS.contains(&a.id));
    for a in &mut ac.attrs {
        if a.id == crate::attrs::attr_id::NAME {
            a.raw_data = encode_name_attr(&r.name_for(owner_uuid));
        }
    }
}

fn scrub_delta(d: &mut pb::AoiSyncDelta, r: &mut Remap) {
    let owner = d.uuid;
    d.uuid = r.uuid(d.uuid);
    if let Some(ac) = &mut d.attrs {
        scrub_attrs(ac, r, owner);
    }
    if let Some(se) = &mut d.skill_effects {
        for dm in &mut se.damages {
            dm.attacker_uuid = r.uuid(dm.attacker_uuid);
            dm.top_summoner_id = r.uuid(dm.top_summoner_id);
        }
    }
}

/// Returns the sanitized, re-encoded payload for one record, or `None` for
/// an opcode we do not model (dropped from the output entirely) or a
/// payload that fails to decode against the modeled schema.
pub fn sanitize(method_id: u32, payload: &[u8], r: &mut Remap) -> Option<Vec<u8>> {
    match method_id {
        opcode::SYNC_NEAR_ENTITIES => {
            let mut m = pb::SyncNearEntities::decode(payload).ok()?;
            for e in &mut m.appear {
                let owner = e.uuid;
                e.uuid = r.uuid(e.uuid);
                if let Some(ac) = &mut e.attrs {
                    scrub_attrs(ac, r, owner);
                }
            }
            for d in &mut m.disappear {
                d.uuid = r.uuid(d.uuid);
            }
            Some(m.encode_to_vec())
        }
        opcode::SYNC_NEAR_DELTA_INFO => {
            let mut m = pb::SyncNearDeltaInfo::decode(payload).ok()?;
            for d in &mut m.delta_infos {
                scrub_delta(d, r);
            }
            Some(m.encode_to_vec())
        }
        opcode::SYNC_TO_ME_DELTA_INFO => {
            let mut m = pb::SyncToMeDeltaInfo::decode(payload).ok()?;
            if let Some(di) = &mut m.delta_info {
                di.uuid = r.uuid(di.uuid);
                if let Some(bd) = &mut di.base_delta {
                    scrub_delta(bd, r);
                }
            }
            Some(m.encode_to_vec())
        }
        opcode::SYNC_CONTAINER_DATA => {
            let mut m = pb::SyncContainerData::decode(payload).ok()?;
            if let Some(v) = &mut m.v_data {
                // TRAP: `char_id` is a *bare* uid, not a packed uuid (see
                // `decode::on_sync_container_data`) — remapping it with the
                // uuid rule silently misattributes the local player (damage
                // numbers stay byte-identical, only class/ability_score go
                // missing).
                v.char_id = r.uid(v.char_id);
                if let Some(cb) = &mut v.char_base {
                    cb.char_id = r.uid(cb.char_id);
                    if !cb.name.is_empty() {
                        cb.name = format!("Player{}", cb.char_id);
                    }
                }
            }
            Some(m.encode_to_vec())
        }
        opcode::ENTER_SCENE => {
            let mut m = pb::EnterScene::decode(payload).ok()?;
            if let Some(info) = &mut m.info
                && let Some(ac) = &mut info.attrs
            {
                scrub_attrs(ac, r, 0);
            }
            Some(m.encode_to_vec())
        }
        // issue #139: dungeon-state/objective/var traffic carries no
        // player-identifying data — verified against all 392 real `0x18`
        // capture messages, whose only strings are var names like
        // `ProgressState`. Nothing to scrub; decode + re-encode through the
        // modeled schema is enough to satisfy the whitelist-by-re-encode
        // safety property above.
        opcode::SYNC_DUNGEON_DATA => {
            let m = pb::DungeonSyncData::decode(payload).ok()?;
            Some(m.encode_to_vec())
        }
        opcode::SYNC_DUNGEON_DIRTY_DATA => {
            let m = pb::SyncDungeonDirtyData::decode(payload).ok()?;
            Some(m.encode_to_vec())
        }
        _ => None,
    }
}

/// The seven opcodes `pb.rs` models. Every other opcode is dropped entirely
/// by [`sanitize`] (and by [`Sanitizer::sanitize_record`]).
pub fn is_modeled(method_id: u32) -> bool {
    matches!(
        method_id,
        opcode::SYNC_NEAR_ENTITIES
            | opcode::SYNC_CONTAINER_DATA
            | opcode::SYNC_NEAR_DELTA_INFO
            | opcode::SYNC_TO_ME_DELTA_INFO
            | opcode::ENTER_SCENE
            | opcode::SYNC_DUNGEON_DATA
            | opcode::SYNC_DUNGEON_DIRTY_DATA
    )
}

/// Stateful, per-session wrapper over [`Remap`]/[`sanitize`] for a live
/// writer (`crates/app/src/dump.rs`) that scrubs each record as it's
/// captured rather than in a separate batch pass. One `Sanitizer` per dump
/// file — its `Remap` must stay alive for the file's whole lifetime so the
/// same player gets the same `PlayerNNNNN` placeholder in every record.
pub struct Sanitizer {
    remap: Remap,
}

impl Default for Sanitizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sanitizer {
    pub fn new() -> Self {
        Self {
            remap: Remap::new(),
        }
    }

    /// Sanitizes one dump record, or returns `None` to mean "drop it
    /// entirely" — either its payload was never decoded (raw, still-
    /// compressed bytes, `payload_decoded: false`, nothing to whitelist) or
    /// its opcode isn't one of the seven [`is_modeled`] schemas, so nothing
    /// about it has been verified free of identifying data. This is the
    /// same drop-what-isn't-whitelisted behavior `sanitize-dump` applies in
    /// its pre-filter, just record-at-a-time instead of over a whole window
    /// up front.
    pub fn sanitize_record(&mut self, rec: &DumpRecord) -> Option<DumpRecord> {
        if !rec.payload_decoded {
            return None;
        }
        let payload = sanitize(rec.method_id, &rec.payload, &mut self.remap)?;
        Some(DumpRecord {
            ts_ms: rec.ts_ms,
            service_uuid: rec.service_uuid,
            method_id: rec.method_id,
            payload,
            payload_decoded: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER_UUID: i64 = (7i64 << 16) | 640;

    #[test]
    fn uuid_remap_preserves_low_16_bits_and_is_stable() {
        let mut r = Remap::new();
        let a = r.uuid(PLAYER_UUID);
        let b = r.uuid(PLAYER_UUID);
        assert_eq!(
            a, b,
            "same input uuid must remap to the same output every time"
        );
        assert_eq!(
            a & 0xFFFF,
            640,
            "low 16 bits (entity kind) must survive unchanged"
        );
        assert_ne!(
            a >> 16,
            7,
            "the uid half must actually be remapped, not passed through"
        );
    }

    #[test]
    fn uuid_remap_of_zero_is_zero() {
        let mut r = Remap::new();
        assert_eq!(r.uuid(0), 0);
    }

    #[test]
    fn bare_uid_remap_is_stable_and_unbounded_by_the_16_bit_low_half() {
        let mut r = Remap::new();
        // A packed uuid's low 16 bits are entity-kind flags, so `uuid()`
        // never lets its output's low 16 bits move independently of the
        // input's. A bare uid has no such structure: remapping a large,
        // arbitrary-looking uid must be able to land anywhere, and must not
        // silently reuse the input's own low 16 bits the way a packed
        // uuid's remap would.
        let uid = 783_903i64; // low 16 bits: 0x0BBF = 3007
        let a = r.uid(uid);
        let b = r.uid(uid);
        assert_eq!(
            a, b,
            "same input uid must remap to the same output every time"
        );
        assert_ne!(
            a & 0xFFFF,
            uid & 0xFFFF,
            "a bare uid's low 16 bits must not be pinned through the remap the way a packed \
             uuid's are — that pinning is `uuid()`-specific structure that does not apply here"
        );
    }

    #[test]
    fn packed_uuid_and_bare_uid_rules_diverge_on_the_same_char_id() {
        // The trap this test guards: CharSerialize.char_id/CharBaseInfo.char_id
        // are bare uids, but every Entity/AoiSyncDelta/SyncDamageInfo uuid
        // field is packed. Applying the uuid rule to a char_id treats its
        // low 16 bits as a packed entity-kind tag and strips them off
        // entirely (`>> 16`) before remapping — corrupting the id.
        let char_id = PLAYER_UUID; // deliberately reuse a "packed-looking" value
        let mut r = Remap::new();
        let via_bare_rule = r.uid(char_id);
        let via_wrong_uuid_rule = r.uuid(char_id) >> 16;
        assert_ne!(
            via_bare_rule, via_wrong_uuid_rule,
            "using the packed-uuid rule on a bare char_id must diverge from the correct \
             bare-uid remap — this is the exact bug the mandatory self-check exists to catch"
        );
    }

    #[test]
    fn scrub_attrs_drops_unlisted_attr_ids_and_keeps_the_whitelist() {
        let mut r = Remap::new();
        let mut ac = pb::AttrCollection {
            uuid: PLAYER_UUID,
            attrs: vec![
                pb::Attr {
                    id: crate::attrs::attr_id::NAME,
                    raw_data: encode_name_attr("RealName"),
                },
                pb::Attr {
                    id: crate::attrs::attr_id::HP,
                    raw_data: vec![1, 2, 3],
                },
                pb::Attr {
                    id: 0x9999, // not in KEEP_ATTRS
                    raw_data: b"unmodeled".to_vec(),
                },
            ],
        };
        scrub_attrs(&mut ac, &mut r, PLAYER_UUID);
        assert_eq!(ac.attrs.len(), 2, "the unlisted attr id must be dropped");
        assert!(ac.attrs.iter().any(|a| a.id == crate::attrs::attr_id::HP));
        let name_attr = ac
            .attrs
            .iter()
            .find(|a| a.id == crate::attrs::attr_id::NAME)
            .expect("NAME attr must survive");
        // raw_data is a varint length prefix + UTF-8 bytes; the encoded name
        // must not contain the original real name.
        let text = String::from_utf8_lossy(&name_attr.raw_data);
        assert!(!text.contains("RealName"));
        assert!(text.contains("Player"));
    }

    #[test]
    fn is_modeled_accepts_exactly_the_seven_known_opcodes() {
        assert!(is_modeled(opcode::SYNC_NEAR_ENTITIES));
        assert!(is_modeled(opcode::SYNC_CONTAINER_DATA));
        assert!(is_modeled(opcode::SYNC_NEAR_DELTA_INFO));
        assert!(is_modeled(opcode::SYNC_TO_ME_DELTA_INFO));
        assert!(is_modeled(opcode::ENTER_SCENE));
        // issue #139: 0x17/0x18 pass through too now — dungeon blobs carry
        // no player-identifying data (see `sanitize`'s doc comment on these
        // two arms).
        assert!(is_modeled(opcode::SYNC_DUNGEON_DATA));
        assert!(is_modeled(opcode::SYNC_DUNGEON_DIRTY_DATA));
        assert!(!is_modeled(0x1234));
    }

    #[test]
    fn sync_container_data_uses_the_bare_uid_rule_for_char_id() {
        let mut r = Remap::new();
        let char_id = 1_646_812i64;
        let msg = pb::SyncContainerData {
            v_data: Some(pb::CharSerialize {
                char_id,
                char_base: Some(pb::CharBaseInfo {
                    char_id,
                    name: "RealPlayerName".to_string(),
                    fight_point: 12345,
                }),
                scene_data: None,
                profession_list: None,
            }),
        };
        let payload = msg.encode_to_vec();
        let out = sanitize(opcode::SYNC_CONTAINER_DATA, &payload, &mut r)
            .expect("SyncContainerData must sanitize");
        let decoded = pb::SyncContainerData::decode(out.as_slice()).unwrap();
        let v = decoded.v_data.expect("v_data must survive");
        let cb = v.char_base.expect("char_base must survive");
        // fight_point (a numeric field, not identity) must be preserved
        // byte-for-byte — this is the regression the char_id bug hid
        // behind: damage numbers stayed correct while identity fields
        // silently went missing.
        assert_eq!(cb.fight_point, 12345);
        assert_eq!(
            v.char_id, cb.char_id,
            "both char_id copies must remap identically"
        );
        assert_ne!(v.char_id, char_id, "char_id must actually be remapped");
        assert!(cb.name.contains(&v.char_id.to_string()));
        assert!(!cb.name.contains("RealPlayerName"));
    }

    // -- Sanitizer (issue #346: the live-writer entry point) ---------------

    fn container_record(ts_ms: u64, char_id: i64, name: &str) -> DumpRecord {
        let msg = pb::SyncContainerData {
            v_data: Some(pb::CharSerialize {
                char_id,
                char_base: Some(pb::CharBaseInfo {
                    char_id,
                    name: name.to_string(),
                    fight_point: 999,
                }),
                scene_data: None,
                profession_list: None,
            }),
        };
        DumpRecord {
            ts_ms,
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_CONTAINER_DATA,
            payload: msg.encode_to_vec(),
            payload_decoded: true,
        }
    }

    #[test]
    fn sanitizer_replaces_names_with_stable_pseudonyms() {
        let mut s = Sanitizer::new();
        let rec = container_record(1, 1_646_812, "RealPlayerName");
        let out = s
            .sanitize_record(&rec)
            .expect("modeled opcode must sanitize");
        let decoded = pb::SyncContainerData::decode(out.payload.as_slice()).unwrap();
        let name = decoded.v_data.unwrap().char_base.unwrap().name;
        assert!(!name.contains("RealPlayerName"));
        assert!(name.starts_with("Player"));
    }

    #[test]
    fn sanitizer_uid_remap_is_consistent_across_records() {
        let mut s = Sanitizer::new();
        let rec_a = container_record(1, 42, "Alice");
        let rec_b = container_record(2, 42, "Alice");
        let out_a = s.sanitize_record(&rec_a).unwrap();
        let out_b = s.sanitize_record(&rec_b).unwrap();
        let name_a = pb::SyncContainerData::decode(out_a.payload.as_slice())
            .unwrap()
            .v_data
            .unwrap()
            .char_base
            .unwrap()
            .name;
        let name_b = pb::SyncContainerData::decode(out_b.payload.as_slice())
            .unwrap()
            .v_data
            .unwrap()
            .char_base
            .unwrap()
            .name;
        assert_eq!(
            name_a, name_b,
            "the same source uid must map to the same pseudonym in every record"
        );
    }

    #[test]
    fn sanitizer_leaves_non_identifying_fields_unchanged() {
        let mut s = Sanitizer::new();
        let rec = container_record(1, 7, "Bob");
        let out = s.sanitize_record(&rec).unwrap();
        let decoded = pb::SyncContainerData::decode(out.payload.as_slice()).unwrap();
        assert_eq!(
            decoded.v_data.unwrap().char_base.unwrap().fight_point,
            999,
            "numeric telemetry fields must survive sanitization byte-for-byte"
        );
        assert_eq!(out.ts_ms, rec.ts_ms);
        assert_eq!(out.service_uuid, rec.service_uuid);
        assert_eq!(out.method_id, rec.method_id);
        assert!(out.payload_decoded);
    }

    #[test]
    fn sanitizer_drops_records_whose_payload_was_never_decoded() {
        let mut s = Sanitizer::new();
        let rec = DumpRecord {
            ts_ms: 1,
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_CONTAINER_DATA,
            payload: vec![0xFF, 0xFE], // raw compressed bytes, not protobuf
            payload_decoded: false,
        };
        assert!(s.sanitize_record(&rec).is_none());
    }

    #[test]
    fn sanitizer_drops_unmodeled_opcodes() {
        let mut s = Sanitizer::new();
        let rec = DumpRecord {
            ts_ms: 1,
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: 0x1234,
            payload: vec![1, 2, 3],
            payload_decoded: true,
        };
        assert!(s.sanitize_record(&rec).is_none());
    }
}
