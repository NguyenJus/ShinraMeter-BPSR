//! Attribute id constants and varint/name decoding (plan §0.6).
//!
//! Every function here is non-panicking: an empty `raw_data`, `id == 0`, or a
//! malformed varint/utf8 payload is skipped rather than propagated as an
//! error.

use std::io::Cursor;

use crate::event::{EnemyHp, PlayerInfo};
use crate::inspect::InspectSink;
use crate::pb;

pub mod attr_id {
    pub const NAME: i32 = 0x01;
    /// `AttrId` (10) — the entity's *type* id, which for a monster is its
    /// row in the game's monster table (what `tables::monster_name` and
    /// `tables::is_boss_monster` key off). Despite the generic name there
    /// is no separate "monster config id" attr: BPSR-ZDPS is explicit that
    /// this is the one that resolves, and rewrites a non-player's uid with
    /// it (`Managers/EncounterManager.cs:637-641`, "*Only the Attribute
    /// named Id (AttrId) is their real type UID which can be resolved into
    /// a name*"). `AttrConfigUid`/`AttrTableUid` exist in the enum but no
    /// reference tracker reads either as a monster identity.
    ///
    /// Corroborated (issue #76) across all four reference trackers:
    /// BPSR-ZDPS `BPSR-ZDPSLib/protos/EnumEAttrType.cs:798`
    /// (`AttrId = 10`), StarResonanceDamageCounter `algo/packet.js:104`,
    /// bpsr-logs `src-tauri/src/protocol/constants.rs:42`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:545`.
    pub const MONSTER_ID: i32 = 0x0A;
    /// `AttrHp` (11310) — current HP. Typed `int64` by the game's own
    /// shipped `Data/FightAttrTable.json:1098-1103` (`"EnumName":
    /// "AttrHp"`, `"IsSyncAoi": true`), so it is decoded at full width
    /// here; the oldest reference tracker narrows it to `int32`, which
    /// corrupts exactly the high-HP raid bosses this meter is for.
    ///
    /// Corroborated (issue #76): BPSR-ZDPS `EnumEAttrType.cs:1306`,
    /// StarResonanceDamageCounter `algo/packet.js:111`, bpsr-logs
    /// `src-tauri/src/protocol/constants.rs:44`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:585`.
    pub const HP: i32 = 0x2C2E;
    /// `AttrMaxHp` (11320) — max HP, read straight off this same attr
    /// channel by every reference tracker (none derives it from a
    /// percentage). Also `int64` per `Data/FightAttrTable.json:1138-1148`.
    /// Not to be confused with `AttrMaxHpTotal` (11321, `0x2C39`), the
    /// rollup of this attr, which no tracker reads.
    ///
    /// Corroborated (issue #76): BPSR-ZDPS `EnumEAttrType.cs:1307`,
    /// StarResonanceDamageCounter `algo/packet.js:112`, bpsr-logs
    /// `src-tauri/src/protocol/constants.rs:45`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:586`.
    ///
    /// Note this attr rides the *appear* packet, not the HP deltas that
    /// follow it — see `encounter::Meter::recompute_boss` for why the boss
    /// heuristic must not require it.
    pub const MAX_HP: i32 = 0x2C38;
    pub const PROFESSION_ID: i32 = 0xDC;
    pub const FIGHT_POINT: i32 = 0x272E;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonLevel = 10070`) because no packet capture was available.
    /// `FIGHT_POINT` is the only id in this module confirmed against
    /// *captured traffic* per `docs/packet-inspection.md`'s "Recording a
    /// result" convention. `MONSTER_ID`, `HP`, and `MAX_HP` (above) are
    /// confirmed by source corroboration instead — all four reference
    /// trackers plus the game's own shipped `FightAttrTable.json`, cited
    /// per-constant above (issue #76). This constant and `SEASON_STRENGTH`
    /// (below) rest on BPSR-ZDPS alone and remain single-source; re-verify
    /// them against a real capture if one ever becomes available.
    pub const SEASON_LEVEL: i32 = 0x2756;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonStrength = 11440`) because no packet capture was
    /// available. See `SEASON_LEVEL`'s doc comment for the full caveat.
    pub const SEASON_STRENGTH: i32 = 0x2CB0;
}

/// protobuf varint → `u64`; `None` on empty/malformed input. The widest
/// lossless form — the typed helpers below narrow it with checked casts so an
/// out-of-range value is rejected rather than silently truncated or
/// reinterpreted.
pub fn decode_varint_u64(raw: &[u8]) -> Option<u64> {
    prost::encoding::decode_varint(&mut Cursor::new(raw)).ok()
}

/// protobuf varint → `i64`; `None` on empty/malformed input or a value above
/// `i64::MAX` (which would otherwise read back as a negative number).
pub fn decode_varint_i64(raw: &[u8]) -> Option<i64> {
    decode_varint_u64(raw).and_then(|v| i64::try_from(v).ok())
}

/// protobuf varint → `i32`; `None` on empty/malformed input or a value
/// outside `i32` range (truncating it would yield a plausible-but-wrong id).
pub fn decode_varint_i32(raw: &[u8]) -> Option<i32> {
    decode_varint_u64(raw).and_then(|v| i32::try_from(v).ok())
}

/// protobuf varint → `u32`; `None` on empty/malformed input or a value
/// outside `u32` range (truncating it would yield a plausible-but-wrong id).
pub fn decode_varint_u32(raw: &[u8]) -> Option<u32> {
    decode_varint_u64(raw).and_then(|v| u32::try_from(v).ok())
}

/// `raw_data` for a name attr is the string bytes behind a protobuf varint
/// length prefix. When that prefix is self-consistent it is honoured (so a
/// 128+ byte name, whose prefix is two bytes, keeps no stray byte); otherwise
/// a single leading byte is dropped, matching the servers that prepend one
/// opaque tag byte. Invalid UTF-8 or an empty result → `None`, so the caller
/// falls back to `Player <uid>` instead of rendering a blank name.
pub fn decode_name(raw: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(raw);
    let bytes = match prost::encoding::decode_varint(&mut cursor) {
        Ok(len) => {
            let rest = &raw[cursor.position() as usize..];
            if len == rest.len() as u64 {
                rest
            } else {
                raw.get(1..)?
            }
        }
        Err(_) => raw.get(1..)?,
    };
    if bytes.is_empty() {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// Builds a `PlayerInfo` from an entity's `Attr` list, reading `NAME`,
/// `PROFESSION_ID`, `FIGHT_POINT` (ability score), `SEASON_LEVEL`, and
/// `SEASON_STRENGTH`. Empty `raw_data` and `id == 0` are always skipped
/// outright — no decoding, no sink call. Every other id, when `sink` is set
/// (issue #25 diagnostic mode), is reported via `InspectSink::on_attr` —
/// known ids (the five above) as well as unknown ones, each tagged with
/// whether we decode it, instead of only unknown ids reaching the sink
/// (slice B widened this from an unknowns-only hook).
pub fn player_info_from_attrs(
    uid: i64,
    attrs: &[pb::Attr],
    sink: Option<&dyn InspectSink>,
) -> PlayerInfo {
    let mut name = None;
    let mut class = None;
    let mut ability_score = None;
    let mut season_level = None;
    let mut season_strength = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        if let Some(sink) = sink {
            let known = matches!(
                attr.id,
                attr_id::NAME
                    | attr_id::PROFESSION_ID
                    | attr_id::FIGHT_POINT
                    | attr_id::SEASON_LEVEL
                    | attr_id::SEASON_STRENGTH
            );
            sink.on_attr(uid, attr.id, &attr.raw_data, known);
        }
        match attr.id {
            attr_id::NAME => {
                if let Some(n) = decode_name(&attr.raw_data) {
                    name = Some(n);
                }
            }
            attr_id::PROFESSION_ID => {
                if let Some(id) = decode_varint_i32(&attr.raw_data) {
                    // An Imagine transform id (issue #37) yields `None` here
                    // rather than overwriting `class` with `Unknown` — see
                    // `pb::class_of_profession_id`'s doc comment.
                    if let Some(c) = pb::class_of_profession_id(id) {
                        class = Some(c);
                    }
                }
            }
            attr_id::FIGHT_POINT => {
                // A raw value of 0 is the wire default when the server hasn't
                // populated the field, not a real ability score — treat it as
                // absent, matching `SyncContainerData`'s `char_base.fight_point
                // > 0` guard in decode.rs.
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    ability_score = Some(v);
                }
            }
            attr_id::SEASON_LEVEL => {
                // Same absent-vs-zero treatment as FIGHT_POINT above.
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    season_level = Some(v);
                }
            }
            attr_id::SEASON_STRENGTH => {
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    season_strength = Some(v);
                }
            }
            _ => {}
        }
    }
    PlayerInfo {
        uid,
        name,
        class,
        ability_score,
        season_level,
        season_strength,
    }
}

/// Builds an `EnemyHp` from an entity's `Attr` list, reading `HP`, `MAX_HP`,
/// and `MONSTER_ID`. Empty `raw_data` and `id == 0` are always skipped
/// outright — no decoding, no sink call. Every other id, when `sink` is set
/// (issue #25 diagnostic mode), is reported via `InspectSink::on_attr` —
/// known ids (the three above) as well as unknown ones, each tagged with
/// whether we decode it, exactly as `player_info_from_attrs` does: an attr
/// id is no less worth discovering for sitting on an enemy entity.
pub fn enemy_hp_from_attrs(
    uid: i64,
    attrs: &[pb::Attr],
    now_ms: u64,
    sink: Option<&dyn InspectSink>,
) -> EnemyHp {
    let mut curr_hp = None;
    let mut max_hp = None;
    let mut monster_id = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        if let Some(sink) = sink {
            let known = matches!(attr.id, attr_id::HP | attr_id::MAX_HP | attr_id::MONSTER_ID);
            sink.on_attr(uid, attr.id, &attr.raw_data, known);
        }
        match attr.id {
            attr_id::HP => {
                if let Some(v) = decode_varint_u64(&attr.raw_data) {
                    curr_hp = Some(v);
                }
            }
            attr_id::MAX_HP => {
                if let Some(v) = decode_varint_u64(&attr.raw_data) {
                    max_hp = Some(v);
                }
            }
            attr_id::MONSTER_ID => {
                if let Some(v) = decode_varint_u32(&attr.raw_data) {
                    monster_id = Some(v);
                }
            }
            _ => {}
        }
    }
    EnemyHp {
        uid,
        curr_hp,
        max_hp,
        monster_id,
        timestamp_ms: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_attr_drops_stray_first_byte() {
        let raw = [0xFFu8, b'H', b'i'];
        assert_eq!(decode_name(&raw), Some("Hi".to_string()));
    }

    #[test]
    fn name_attr_invalid_utf8_is_none() {
        let raw = [0x00u8, 0xFF, 0xFE];
        assert_eq!(decode_name(&raw), None);
    }

    #[test]
    fn varint_hp_decodes() {
        let mut buf = Vec::new();
        prost::encoding::encode_varint(123456u64, &mut buf);
        assert_eq!(decode_varint_i64(&buf), Some(123456));
    }

    #[test]
    fn empty_raw_data_skipped() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: Vec::new(),
        }];
        let hp = enemy_hp_from_attrs(1, &attrs, 0, None);
        assert_eq!(hp.curr_hp, None);
    }

    fn varint(v: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        prost::encoding::encode_varint(v, &mut buf);
        buf
    }

    #[test]
    fn varint_out_of_i32_range_is_rejected_not_truncated() {
        let raw = varint(0x1_0000_0001);
        assert_eq!(decode_varint_i32(&raw), None);
    }

    #[test]
    fn varint_above_i64_max_is_rejected_not_reinterpreted() {
        let raw = varint(u64::MAX);
        assert_eq!(decode_varint_i64(&raw), None);
    }

    #[test]
    fn huge_hp_varint_is_not_clamped_to_zero() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: varint(u64::MAX),
        }];
        let hp = enemy_hp_from_attrs(1, &attrs, 0, None);
        assert_eq!(hp.curr_hp, Some(u64::MAX));
    }

    #[test]
    fn out_of_range_profession_id_yields_no_class() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(0x1_0000_0001),
        }];
        assert_eq!(player_info_from_attrs(1, &attrs, None).class, None);
    }

    // -- Imagine profession ids (issue #37) --------------------------------

    #[test]
    fn imagine_profession_id_yields_no_class() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(8), // Dorothy
        }];
        assert_eq!(player_info_from_attrs(1, &attrs, None).class, None);
    }

    #[test]
    fn imagine_profession_id_does_not_clobber_a_class_read_earlier_in_the_same_attr_list() {
        let attrs = vec![
            pb::Attr {
                id: attr_id::PROFESSION_ID,
                raw_data: varint(1), // Stormblade
            },
            pb::Attr {
                id: attr_id::PROFESSION_ID,
                raw_data: varint(8), // Dorothy (Imagine) — must not overwrite
            },
        ];
        assert_eq!(
            player_info_from_attrs(1, &attrs, None).class,
            Some(pb::Class::Stormblade)
        );
    }

    /// issue #76: the whole monster attr triple, built from the reference
    /// trackers' own field numbering, decodes to a fully-populated
    /// `EnemyHp`. The literal ids are deliberate — this test pins the
    /// constants' *values*, so renumbering `attr_id` fails here rather
    /// than silently returning `None` for every monster on the wire.
    ///
    /// `AttrId = 10` / `AttrHp = 11310` / `AttrMaxHp = 11320`, corroborated
    /// by all four reference trackers; see the `attr_id` doc comments for
    /// the per-repo file/line citations.
    #[test]
    fn monster_attr_triple_decodes_to_a_full_enemy_hp() {
        let attrs = vec![
            pb::Attr {
                id: 0x0A,
                raw_data: varint(103),
            },
            pb::Attr {
                id: 0x2C2E,
                raw_data: varint(4_200_000),
            },
            pb::Attr {
                id: 0x2C38,
                raw_data: varint(9_000_000),
            },
        ];

        let hp = enemy_hp_from_attrs(42, &attrs, 7, None);

        assert_eq!(hp.uid, 42);
        assert_eq!(hp.monster_id, Some(103));
        assert_eq!(hp.curr_hp, Some(4_200_000));
        assert_eq!(hp.max_hp, Some(9_000_000));
        assert_eq!(hp.timestamp_ms, 7);
    }

    /// The game's own `FightAttrTable.json` types `AttrHp`/`AttrMaxHp` as
    /// `int64`, and BPSR-ZDPS reads both with `ReadInt64()`. A raid boss's
    /// max HP genuinely exceeds `i32`, so narrowing here (as the oldest
    /// reference tracker does with `reader.int32()`) would corrupt exactly
    /// the fights this meter exists for.
    #[test]
    fn max_hp_above_i32_range_decodes_losslessly() {
        let attrs = vec![pb::Attr {
            id: attr_id::MAX_HP,
            raw_data: varint(9_000_000_000),
        }];
        assert_eq!(
            enemy_hp_from_attrs(1, &attrs, 0, None).max_hp,
            Some(9_000_000_000)
        );
    }

    #[test]
    fn out_of_range_monster_id_is_rejected() {
        let attrs = vec![pb::Attr {
            id: attr_id::MONSTER_ID,
            raw_data: varint(u64::from(u32::MAX) + 1),
        }];
        assert_eq!(enemy_hp_from_attrs(1, &attrs, 0, None).monster_id, None);
    }

    #[test]
    fn single_byte_name_is_none_not_empty_string() {
        assert_eq!(decode_name(&[0x00]), None);
    }

    #[test]
    fn long_name_keeps_no_stray_prefix_byte() {
        let name = "a".repeat(130);
        let mut raw = varint(name.len() as u64); // 2-byte length varint
        assert_eq!(raw.len(), 2);
        raw.extend_from_slice(name.as_bytes());
        assert_eq!(decode_name(&raw), Some(name));
    }

    #[test]
    fn fight_point_attr_sets_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(123_456),
        }];
        assert_eq!(
            player_info_from_attrs(1, &attrs, None).ability_score,
            Some(123_456)
        );
    }

    #[test]
    fn zero_fight_point_yields_no_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(0),
        }];
        assert_eq!(player_info_from_attrs(1, &attrs, None).ability_score, None);
    }

    #[test]
    fn missing_fight_point_attr_yields_no_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(1),
        }];
        assert_eq!(player_info_from_attrs(1, &attrs, None).ability_score, None);
    }

    #[test]
    fn out_of_range_fight_point_is_rejected_not_truncated() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(0x1_0000_0001),
        }];
        assert_eq!(player_info_from_attrs(1, &attrs, None).ability_score, None);
    }

    #[test]
    fn unknown_attr_id_ignored() {
        let mut raw = Vec::new();
        prost::encoding::encode_varint(999u64, &mut raw);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw,
        }];
        let info = player_info_from_attrs(1, &attrs, None);
        assert_eq!(info.name, None);
        assert_eq!(info.class, None);
    }

    // -- InspectSink observation (issue #25 slice A) ----------------------

    /// Test-only `InspectSink` that just records every `on_attr` call, in
    /// order, for assertions.
    /// `(uid, attr_id, raw, known)`.
    type RecordedAttr = (i64, i32, Vec<u8>, bool);

    struct RecordingSink {
        attrs: std::sync::Mutex<Vec<RecordedAttr>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                attrs: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl InspectSink for RecordingSink {
        fn on_notify(
            &self,
            _service_uuid: u64,
            _method_id: u32,
            _payload: &[u8],
            _payload_decoded: bool,
            _now_ms: u64,
        ) {
        }

        fn on_attr(&self, uid: i64, attr_id: i32, raw: &[u8], known: bool) {
            self.attrs
                .lock()
                .unwrap()
                .push((uid, attr_id, raw.to_vec(), known));
        }
    }

    #[test]
    fn unknown_attr_id_is_observed_via_sink_as_not_known() {
        let raw = varint(999);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw.clone(),
        }];
        let sink = RecordingSink::new();

        let info = player_info_from_attrs(7, &attrs, Some(&sink));

        assert_eq!(info.name, None);
        assert_eq!(*sink.attrs.lock().unwrap(), vec![(7, 0x9999, raw, false)]);
    }

    #[test]
    fn known_attr_id_is_observed_via_sink_as_known() {
        let attrs = vec![pb::Attr {
            id: attr_id::NAME,
            raw_data: vec![0xFF, b'H', b'i'],
        }];
        let sink = RecordingSink::new();

        let info = player_info_from_attrs(1, &attrs, Some(&sink));

        assert_eq!(info.name.as_deref(), Some("Hi"));
        assert_eq!(
            *sink.attrs.lock().unwrap(),
            vec![(1, attr_id::NAME, vec![0xFF, b'H', b'i'], true)]
        );
    }

    #[test]
    fn empty_raw_data_and_zero_id_do_not_trigger_the_attr_sink_call() {
        let attrs = vec![
            pb::Attr {
                id: 0x1234,
                raw_data: Vec::new(),
            },
            pb::Attr {
                id: 0,
                raw_data: varint(1),
            },
        ];
        let sink = RecordingSink::new();

        let _ = player_info_from_attrs(1, &attrs, Some(&sink));

        assert!(sink.attrs.lock().unwrap().is_empty());
    }

    // -- InspectSink observation on enemy entities ------------------------
    //
    // The enemy attr walk reports to the sink exactly as the player one
    // does; an unknown id is no less a discovery for sitting on a monster.

    #[test]
    fn unknown_enemy_attr_id_is_observed_via_sink_as_not_known() {
        let raw = varint(999);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw.clone(),
        }];
        let sink = RecordingSink::new();

        let hp = enemy_hp_from_attrs(7, &attrs, 0, Some(&sink));

        assert_eq!(hp.curr_hp, None);
        assert_eq!(*sink.attrs.lock().unwrap(), vec![(7, 0x9999, raw, false)]);
    }

    #[test]
    fn known_enemy_attr_id_is_observed_via_sink_as_known() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: varint(1234),
        }];
        let sink = RecordingSink::new();

        let hp = enemy_hp_from_attrs(1, &attrs, 0, Some(&sink));

        assert_eq!(hp.curr_hp, Some(1234));
        assert_eq!(
            *sink.attrs.lock().unwrap(),
            vec![(1, attr_id::HP, varint(1234), true)]
        );
    }

    #[test]
    fn empty_raw_data_and_zero_id_do_not_trigger_the_enemy_attr_sink_call() {
        let attrs = vec![
            pb::Attr {
                id: 0x1234,
                raw_data: Vec::new(),
            },
            pb::Attr {
                id: 0,
                raw_data: varint(1),
            },
        ];
        let sink = RecordingSink::new();

        let _ = enemy_hp_from_attrs(1, &attrs, 0, Some(&sink));

        assert!(sink.attrs.lock().unwrap().is_empty());
    }
}
