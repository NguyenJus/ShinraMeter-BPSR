//! Wire-level byte builders for scripting `bpsr-protocol` fixtures, shared
//! across the workspace's crate test suites (promoted from
//! `crates/protocol/tests/common/mod.rs`, plan `docs/plans/system-test-harness.md`
//! §1.3/A2). Every builder emits **big-endian** headers matching the wire
//! format documented in `crates/protocol/src/frame.rs`.
//!
//! This module is deliberately app-free: it produces raw bytes and
//! `bpsr_protocol::pb` messages only, never touching `bpsr-meter` or the app
//! crate. See `crate::scenario` for the higher-level DSL built on top of it.

use crate::scenario::Hit;
use bpsr_protocol::pb;
use prost::Message;

/// Packs a player uid into the wire uuid layout (`event.rs`):
/// `uid << 16 | is_summon << 15 | is_client << 14 | entity_type << 6`, with
/// `entity_type = 10` (player) giving low bits `640`.
pub const fn player_uuid(uid: i64) -> i64 {
    (uid << 16) | 640
}

/// Packs a monster uid into the wire uuid layout, `entity_type = 1`
/// (monster) giving low bits `64`.
pub const fn monster_uuid(uid: i64) -> i64 {
    (uid << 16) | 64
}

/// Profession ids -> `bpsr_protocol::pb::Class` (`crates/protocol/src/pb.rs`).
pub mod prof {
    pub const STORMBLADE: i32 = 1;
    pub const FROST_MAGE: i32 = 2;
    pub const TWIN_STRIKER: i32 = 3;
    pub const WIND_KNIGHT: i32 = 4;
    pub const VERDANT_ORACLE: i32 = 5;
    pub const HEAVY_GUARDIAN: i32 = 9;
    pub const MARKSMAN: i32 = 11;
    pub const SHIELD_KNIGHT: i32 = 12;
    pub const BEAT_PERFORMER: i32 = 13;
}

/// Builds one outer frame: `[total_len: BE u32][packet_type: BE u16][body]`.
/// `total_len` includes its own 4 bytes, per the wire format.
pub fn frame(fragment_type: u16, compressed: bool, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let total_len = 4 + 2 + body.len() as u32;
    buf.extend_from_slice(&total_len.to_be_bytes());
    let packet_type = fragment_type
        | if compressed {
            bpsr_protocol::frame::COMPRESSION_FLAG
        } else {
            0
        };
    buf.extend_from_slice(&packet_type.to_be_bytes());
    buf.extend_from_slice(body);
    buf
}

/// Builds a full outer frame carrying a Notify fragment: `service_uuid` (the
/// crate's `SERVICE_UUID`) + `stub_id` (ignored, always 0) + `method_id` +
/// `payload`, optionally zstd-compressed.
pub fn notify(method_id: u32, payload: &[u8], compressed: bool) -> Vec<u8> {
    notify_with_service(
        bpsr_protocol::frame::SERVICE_UUID,
        method_id,
        payload,
        compressed,
    )
}

/// Like [`notify`], but for an arbitrary `service_uuid` — e.g.
/// `bpsr_protocol::frame::TEAM_NTF_SERVICE_UUID` (issue #146) — instead of
/// always the crate's main `SERVICE_UUID`.
pub fn notify_with_service(
    service_uuid: u64,
    method_id: u32,
    payload: &[u8],
    compressed: bool,
) -> Vec<u8> {
    let raw = if compressed {
        zstd::stream::encode_all(payload, 0).expect("zstd encode")
    } else {
        payload.to_vec()
    };
    let mut body = Vec::new();
    body.extend_from_slice(&service_uuid.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // stub_id, ignored by the decoder
    body.extend_from_slice(&method_id.to_be_bytes());
    body.extend_from_slice(&raw);
    frame(2, compressed, &body) // FragmentType::Notify == 2
}

/// Builds a full outer frame carrying a FrameDown fragment: `server_sequence_id`
/// (always 0) + `inner`, optionally zstd-compressed. `inner` is itself a
/// stream of outer frames.
pub fn framedown(inner: &[u8], compressed: bool) -> Vec<u8> {
    let raw = if compressed {
        zstd::stream::encode_all(inner, 0).expect("zstd encode")
    } else {
        inner.to_vec()
    };
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_be_bytes()); // server_sequence_id, ignored
    body.extend_from_slice(&raw);
    frame(6, compressed, &body) // FragmentType::FrameDown == 6
}

/// A `SyncDamageInfo` with sensible non-zero defaults so a test only has to
/// override the fields it cares about.
pub fn base_damage(attacker_uuid: i64, owner_id: i32, value: i64) -> pb::SyncDamageInfo {
    pb::SyncDamageInfo {
        is_miss: false,
        r#type: pb::EDamageType::Normal as i32,
        type_flag: 0,
        value,
        lucky_value: 0,
        hp_lessen_value: value,
        attacker_uuid,
        owner_id,
        is_dead: false,
        top_summoner_id: 0,
    }
}

/// Maps a `scenario::Hit` (meter-level damage description) onto the wire
/// `pb::SyncDamageInfo` shape, per `decode.rs`'s flag semantics
/// (`docs/plans/system-test-harness.md` §0.7).
pub fn damage_info(hit: &Hit) -> pb::SyncDamageInfo {
    pb::SyncDamageInfo {
        is_miss: hit.miss,
        r#type: if hit.miss {
            pb::EDamageType::Miss as i32
        } else if hit.heal {
            pb::EDamageType::Heal as i32
        } else {
            pb::EDamageType::Normal as i32
        },
        type_flag: hit.crit as i32,
        value: if hit.lucky { 0 } else { hit.value },
        lucky_value: if hit.lucky { hit.value } else { 0 },
        hp_lessen_value: hit.value,
        attacker_uuid: player_uuid(hit.attacker_uid),
        owner_id: hit.skill_id,
        is_dead: hit.kills_target,
        top_summoner_id: if hit.summoner_uid != 0 {
            player_uuid(hit.summoner_uid)
        } else {
            0
        },
    }
}

/// Prost-encodes a `SyncNearDeltaInfo` payload (not wrapped in a frame)
/// carrying a single `SyncDamageInfo` targeting `target_uuid`.
pub fn damage_delta(target_uuid: i64, dmg: pb::SyncDamageInfo) -> Vec<u8> {
    damage_delta_multi(target_uuid, vec![dmg])
}

/// Prost-encodes a `SyncNearDeltaInfo` payload (not wrapped in a frame)
/// carrying N damage entries against `target_uuid`, in order.
pub fn damage_delta_multi(target_uuid: i64, dmgs: Vec<pb::SyncDamageInfo>) -> Vec<u8> {
    let delta = pb::AoiSyncDelta {
        uuid: target_uuid,
        attrs: None,
        skill_effects: Some(pb::SkillEffect { damages: dmgs }),
        buff_effect: None,
    };
    let msg = pb::SyncNearDeltaInfo {
        delta_infos: vec![delta],
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    payload
}

/// Prost-encodes a `SyncNearDeltaInfo` payload (not wrapped in a frame)
/// carrying an attr-only delta against `uuid` (e.g. a boss HP update over
/// time).
pub fn attr_delta_payload(uuid: i64, attrs: Vec<pb::Attr>) -> Vec<u8> {
    let delta = pb::AoiSyncDelta {
        uuid,
        attrs: Some(pb::AttrCollection { uuid, attrs }),
        skill_effects: None,
        buff_effect: None,
    };
    let msg = pb::SyncNearDeltaInfo {
        delta_infos: vec![delta],
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    payload
}

/// Builds a `BuffEffect` for `AoiSyncDelta.buff_effect` (issue #267).
/// `event_type` is the wire `EBuffEventType` value (`AddTo`, `Remove`, ...);
/// `base_id`, when `Some`, is carried the same way a real `AddTo` event
/// carries one — double-encoded as a `BuffInfo` inside a
/// `BuffEffectLogicInfo` with `effect_type == pb::BUFF_EFFECT_ADD_BUFF` (see
/// `pb::AoiSyncDelta::buff_effect`'s doc comment for why that indirection is
/// how this build's wire format actually works).
pub fn buff_effect(
    event_type: pb::EBuffEventType,
    buff_uuid: i32,
    host_uuid: i64,
    trigger_time: i64,
    base_id: Option<i32>,
) -> pb::BuffEffect {
    let logic_effect = match base_id {
        Some(base_id) => {
            let info = pb::BuffInfo {
                buff_uuid,
                base_id,
                ..Default::default()
            };
            let mut raw_data = Vec::new();
            info.encode(&mut raw_data).unwrap();
            vec![pb::BuffEffectLogicInfo {
                effect_type: pb::BUFF_EFFECT_ADD_BUFF,
                raw_data,
                is_loop: false,
            }]
        }
        None => Vec::new(),
    };
    pb::BuffEffect {
        r#type: event_type as i32,
        buff_uuid,
        host_uuid,
        trigger_time,
        logic_effect,
    }
}

/// Prost-encodes a `SyncNearDeltaInfo` payload (not wrapped in a frame)
/// carrying `effects` against `target_uuid` (issue #267).
pub fn buff_delta_payload(target_uuid: i64, effects: Vec<pb::BuffEffect>) -> Vec<u8> {
    let delta = pb::AoiSyncDelta {
        uuid: target_uuid,
        attrs: None,
        skill_effects: None,
        buff_effect: Some(pb::BuffEffectSync {
            uuid: 0,
            buff_effects: effects,
        }),
    };
    let msg = pb::SyncNearDeltaInfo {
        delta_infos: vec![delta],
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    payload
}

/// One-shot: a full outer Notify frame carrying a `SyncNearDeltaInfo` with a
/// single damage entry against `target_uuid`.
pub fn damage_notify_frame(target_uuid: i64, dmg: pb::SyncDamageInfo, compressed: bool) -> Vec<u8> {
    let payload = damage_delta(target_uuid, dmg);
    notify(
        bpsr_protocol::decode::opcode::SYNC_NEAR_DELTA_INFO,
        &payload,
        compressed,
    )
}

/// Builds an `Attr` carrying a name, with the stray leading tag byte the
/// server always prepends (see `attrs::decode_name`).
pub fn name_attr(name: &str) -> pb::Attr {
    let mut raw = vec![0xFFu8];
    raw.extend_from_slice(name.as_bytes());
    pb::Attr {
        id: bpsr_protocol::attrs::attr_id::NAME,
        raw_data: raw,
    }
}

/// Builds a varint-encoded `Attr` for any of the numeric attr ids
/// (`MONSTER_ID`, `HP`, `MAX_HP`, `PROFESSION_ID`, `FIGHT_POINT`,
/// `SCENE_BASIC_ID`, ...).
pub fn varint_attr(id: i32, value: u64) -> pb::Attr {
    let mut raw = Vec::new();
    prost::encoding::encode_varint(value, &mut raw);
    pb::Attr { id, raw_data: raw }
}

/// Builds an `Attr` carrying `attr_id::SKILL_LEVEL_ID_LIST` (issue #33): a
/// prost-encoded `SkillLevelIdList` wrapping one `SkillLevelInfo` per id,
/// each with `remodel_level` (tier, issues #169/#170) left at 0. Use
/// [`skill_list_attr_with_tiers`] when a test needs a nonzero tier.
pub fn skill_list_attr(ids: &[i32]) -> pb::Attr {
    let pairs: Vec<(i32, i32)> = ids.iter().map(|&id| (id, 0)).collect();
    skill_list_attr_with_tiers(&pairs)
}

/// Builds an `Attr` carrying `attr_id::SKILL_LEVEL_ID_LIST` (issue #33): a
/// prost-encoded `SkillLevelIdList` wrapping one `SkillLevelInfo` per
/// `(skill_id, remodel_level)` pair — `remodel_level` is the tier field
/// issues #169/#170 thread through (BPSR-ZDPS's `Tier`).
pub fn skill_list_attr_with_tiers(pairs: &[(i32, i32)]) -> pb::Attr {
    let msg = pb::SkillLevelIdList {
        skills: pairs
            .iter()
            .map(|&(skill_id, remodel_level)| pb::SkillLevelInfo {
                skill_id,
                current_level: 1,
                remodel_level,
            })
            .collect(),
    };
    let mut raw = Vec::new();
    msg.encode(&mut raw).unwrap();
    pb::Attr {
        id: bpsr_protocol::attrs::attr_id::SKILL_LEVEL_ID_LIST,
        raw_data: raw,
    }
}

/// Builds an `appear` entity for `SyncNearEntities`.
pub fn appear_entity(uuid: i64, ent_type: i32, attrs: Vec<pb::Attr>) -> pb::Entity {
    pb::Entity {
        uuid,
        ent_type,
        attrs: Some(pb::AttrCollection { uuid, attrs }),
    }
}

/// Prost-encodes a `SyncNearEntities` payload (not wrapped in a frame).
pub fn sync_near_entities_payload(entities: Vec<pb::Entity>) -> Vec<u8> {
    let msg = pb::SyncNearEntities {
        appear: entities,
        disappear: Vec::new(),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

/// Prost-encodes a `SyncNearEntities` payload whose `disappear` list retires
/// `uuids` (issue #215), with no `appear` entities and no tag-2 reason on any
/// entry — see [`sync_near_entities_disappear_typed_payload`] for the latter.
pub fn sync_near_entities_disappear_payload(uuids: &[i64]) -> Vec<u8> {
    sync_near_entities_disappear_typed_payload(
        &uuids.iter().map(|uuid| (*uuid, None)).collect::<Vec<_>>(),
    )
}

/// Prost-encodes a `SyncNearEntities` payload whose `disappear` list retires
/// each `(uuid, disappear_type)` pair (issue #276), with no `appear`
/// entities. A `None` type omits tag 2 entirely, which is what 382 of the
/// 851 disappear entries in our captures look like.
pub fn sync_near_entities_disappear_typed_payload(
    entries: &[(i64, Option<pb::EDisappearType>)],
) -> Vec<u8> {
    let msg = pb::SyncNearEntities {
        appear: Vec::new(),
        disappear: entries
            .iter()
            .map(|(uuid, disappear_type)| pb::DisappearEntity {
                uuid: *uuid,
                disappear_type: disappear_type.map(|t| t as i32),
            })
            .collect(),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

/// Prost-encodes a `SyncContainerData` payload (not wrapped in a frame)
/// carrying a character name + profession id. `char_id` is the **uid**
/// directly (`on_sync_container_data` does not call `uid_of`), unlike the
/// appear path.
pub fn sync_container_data_payload(
    char_id: i64,
    name: &str,
    profession_id: i32,
    fight_point: i32,
) -> Vec<u8> {
    let msg = pb::SyncContainerData {
        v_data: Some(pb::CharSerialize {
            char_id,
            char_base: Some(pb::CharBaseInfo {
                char_id,
                name: name.to_string(),
                fight_point,
            }),
            profession_list: Some(pb::ProfessionList {
                cur_profession_id: profession_id,
            }),
        }),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

/// Prost-encodes an `EnterScene` payload (not wrapped in a frame) carrying
/// only the scene id, via `attr_id::SCENE_BASIC_ID` on the scene's attr
/// channel.
pub fn enter_scene_payload(scene_id: u32) -> Vec<u8> {
    let msg = pb::EnterScene {
        info: Some(pb::EnterSceneInfo {
            attrs: Some(pb::AttrCollection {
                uuid: 0,
                attrs: vec![varint_attr(
                    bpsr_protocol::attrs::attr_id::SCENE_BASIC_ID,
                    scene_id as u64,
                )],
            }),
        }),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

/// Builds a fully-populated `TeamMemData` roster entry (issue #146) — a
/// name, class, and ability score, so a test only has to override what it
/// cares about via the individual fields.
pub fn team_member(
    char_id: i64,
    name: &str,
    profession_id: i32,
    fight_point: i64,
) -> pb::TeamMemData {
    pb::TeamMemData {
        char_id,
        scene_id: 0,
        group_id: 0,
        social_data: Some(pb::TeamMemberSocialData {
            basic_data: Some(pb::TeamBasicData {
                char_id,
                name: name.to_string(),
                level: 0,
            }),
            profession_data: Some(pb::TeamProfessionData { profession_id }),
            user_attr_data: Some(pb::TeamUserAttrData {
                fight_point,
                season_strength: 0,
            }),
        }),
    }
}

/// A roster entry carrying only `char_id` — the "bots are missing a lot of
/// fields" case (issue #146): no `social_data` at all.
pub fn bot_team_member(char_id: i64) -> pb::TeamMemData {
    pb::TeamMemData {
        char_id,
        scene_id: 0,
        group_id: 0,
        social_data: None,
    }
}

/// Prost-encodes a `NotifyJoinTeam` payload (not wrapped in a frame)
/// carrying `members` as the roster.
pub fn notify_join_team_payload(members: Vec<pb::TeamMemData>) -> Vec<u8> {
    let msg = pb::NotifyJoinTeam {
        v_request: Some(pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: members,
        }),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_info_sets_top_summoner_id_for_a_pet_hit() {
        let hit = Hit::new(500, 101, 1_000).by_pet_of(999);
        let info = damage_info(&hit);
        assert_eq!(info.attacker_uuid, player_uuid(500));
        assert_eq!(info.top_summoner_id, player_uuid(999));
    }

    #[test]
    fn damage_info_sets_type_flag_for_a_crit_hit() {
        let hit = Hit::new(500, 101, 1_000).crit();
        let info = damage_info(&hit);
        assert_eq!(info.type_flag, 1);
    }
}
