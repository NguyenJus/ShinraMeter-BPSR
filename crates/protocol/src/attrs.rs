//! Attribute id constants and varint/name decoding (plan §0.6).
//!
//! Every function here is non-panicking: an empty `raw_data`, `id == 0`, or a
//! malformed varint/utf8 payload is skipped rather than propagated as an
//! error.

use std::io::Cursor;

use prost::Message;

use crate::event::{EnemyHp, PlayerInfo};
use crate::inspect::InspectSink;
use crate::pb;

pub mod attr_id {
    pub const NAME: i32 = 0x01;
    /// **Unverified** against live traffic. Critical to issue #76 ("No
    /// target" in header). Confirm via `docs/packet-inspection.md`.
    pub const MONSTER_ID: i32 = 0x0A;
    /// **Unverified** against live traffic. Critical to issue #76 ("No
    /// target" in header). Confirm via `docs/packet-inspection.md`.
    pub const HP: i32 = 0x2C2E;
    /// **Unverified** against live traffic. Critical to issue #76 ("No
    /// target" in header). Confirm via `docs/packet-inspection.md`.
    pub const MAX_HP: i32 = 0x2C38;
    pub const PROFESSION_ID: i32 = 0xDC;
    pub const FIGHT_POINT: i32 = 0x272E;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonLevel = 10070`) because no packet capture was available.
    /// Only `FIGHT_POINT` in this module has been confirmed against captured
    /// traffic per `docs/packet-inspection.md`'s "Recording a result"
    /// convention. The attr ids `MONSTER_ID`, `HP`, `MAX_HP` (above), and
    /// this constant, along with `SEASON_STRENGTH` (below), are unverified
    /// reference-derived guesses. Issue #76 tracks the impact of unverified
    /// `MONSTER_ID` and `MAX_HP`. Re-verify against a real capture if one
    /// ever becomes available.
    pub const SEASON_LEVEL: i32 = 0x2756;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonStrength = 11440`) because no packet capture was
    /// available. See `SEASON_LEVEL`'s doc comment for the full caveat.
    pub const SEASON_STRENGTH: i32 = 0x2CB0;
    /// Reference-derived, **not confirmed against live traffic** (issue
    /// #33): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSkillLevelIdList = 116`) because no packet capture was
    /// available. Same unverified-provenance caveat as `SEASON_LEVEL` above
    /// and `pb::IMAGINE_PROFESSION_IDS`. Re-verify against a real capture if
    /// one ever becomes available.
    pub const SKILL_LEVEL_ID_LIST: i32 = 0x74;
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

/// `raw_data` for `attr_id::SKILL_LEVEL_ID_LIST` → the equipped skill ids, in
/// wire order, with `0` entries dropped (matching the crate's zero-is-absent
/// convention). `None` on malformed input (prost decode failure), per this
/// module's non-panicking convention — `player_info_from_attrs` treats
/// `None` the same as "no ids".
///
/// Uses `pb::SkillLevelIdList`, whose wrapper tag is an unverified constant
/// (see its doc comment). If that tag is wrong, a non-empty `raw` still
/// parses as *zero* skills rather than erroring — a silent, non-crashing
/// miss (issue #33's open question #2) — so that specific case is logged at
/// `debug` to keep the failure diagnosable from a user's log.
pub fn decode_skill_ids(raw: &[u8]) -> Option<Vec<i32>> {
    let list = pb::SkillLevelIdList::decode(raw).ok()?;
    let ids: Vec<i32> = list
        .skills
        .into_iter()
        .map(|s| s.skill_id)
        .filter(|&id| id != 0)
        .collect();
    if !raw.is_empty() && ids.is_empty() {
        log::debug!(
            "attr_id::SKILL_LEVEL_ID_LIST: {} raw bytes decoded to zero skill ids; \
             the wrapper tag (pb::SkillLevelIdList) may be wrong",
            raw.len()
        );
    }
    Some(ids)
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
    let mut skill_ids = Vec::new();
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
                    | attr_id::SKILL_LEVEL_ID_LIST
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
            attr_id::SKILL_LEVEL_ID_LIST => {
                if let Some(ids) = decode_skill_ids(&attr.raw_data) {
                    skill_ids = ids;
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
        skill_ids,
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

    // -- Skill level id list (issue #33) -----------------------------------

    fn encode_skill_list(ids: &[i32]) -> Vec<u8> {
        let msg = pb::SkillLevelIdList {
            skills: ids
                .iter()
                .map(|&skill_id| pb::SkillLevelInfo {
                    skill_id,
                    current_level: 1,
                    remodel_level: 0,
                })
                .collect(),
        };
        let mut buf = Vec::new();
        prost::Message::encode(&msg, &mut buf).unwrap();
        buf
    }

    #[test]
    fn decode_skill_ids_well_formed_payload_decodes_in_order() {
        let raw = encode_skill_list(&[3905, 102640, 71000]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![3905, 102640, 71000]));
    }

    #[test]
    fn decode_skill_ids_empty_payload_yields_empty_list() {
        assert_eq!(decode_skill_ids(&[]), Some(Vec::new()));
    }

    #[test]
    fn decode_skill_ids_truncated_varint_yields_none() {
        // A single 0x80 byte is a continuation bit with nothing following —
        // malformed, must not panic.
        assert_eq!(decode_skill_ids(&[0x80]), None);
    }

    #[test]
    fn decode_skill_ids_garbage_bytes_yield_none() {
        let raw = [0xFFu8, 0x00, 0xAB, 0xCD, 0xEF];
        assert_eq!(decode_skill_ids(&raw), None);
    }

    #[test]
    fn decode_skill_ids_drops_zero_ids() {
        let raw = encode_skill_list(&[3905, 0, 102640]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![3905, 102640]));
    }

    #[test]
    fn skill_level_id_list_attr_sets_skill_ids_on_player_info() {
        let attrs = vec![pb::Attr {
            id: attr_id::SKILL_LEVEL_ID_LIST,
            raw_data: encode_skill_list(&[3905, 102640]),
        }];
        assert_eq!(
            player_info_from_attrs(1, &attrs, None).skill_ids,
            vec![3905, 102640]
        );
    }

    #[test]
    fn missing_skill_level_id_list_attr_yields_empty_skill_ids() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(1),
        }];
        assert_eq!(
            player_info_from_attrs(1, &attrs, None).skill_ids,
            Vec::<i32>::new()
        );
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
