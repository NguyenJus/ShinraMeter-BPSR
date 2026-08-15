//! Shared fixture builders for `bpsr-protocol` integration tests (plan
//! T1.5). Every builder emits **big-endian** headers matching the wire
//! format documented in `crates/protocol/src/frame.rs`. Not every helper is
//! used by every test binary that includes this module, hence the blanket
//! `dead_code` allowance.
#![allow(dead_code)]

use bpsr_protocol::pb;
use prost::Message;

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
    let raw = if compressed {
        zstd::stream::encode_all(payload, 0).expect("zstd encode")
    } else {
        payload.to_vec()
    };
    let mut body = Vec::new();
    body.extend_from_slice(&bpsr_protocol::frame::SERVICE_UUID.to_be_bytes());
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

/// Prost-encodes a `SyncNearDeltaInfo` payload (not wrapped in a frame)
/// carrying a single `SyncDamageInfo` targeting `target_uuid`.
pub fn damage_delta(target_uuid: i64, dmg: pb::SyncDamageInfo) -> Vec<u8> {
    let delta = pb::AoiSyncDelta {
        uuid: target_uuid,
        attrs: None,
        skill_effects: Some(pb::SkillEffect { damages: vec![dmg] }),
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
/// (`MONSTER_ID`, `HP`, `MAX_HP`, `PROFESSION_ID`, `FIGHT_POINT`).
pub fn varint_attr(id: i32, value: u64) -> pb::Attr {
    let mut raw = Vec::new();
    prost::encoding::encode_varint(value, &mut raw);
    pb::Attr { id, raw_data: raw }
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

/// Prost-encodes a `SyncContainerData` payload (not wrapped in a frame)
/// carrying a character name + profession id.
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
            scene_data: None,
            profession_list: Some(pb::ProfessionList {
                cur_profession_id: profession_id,
            }),
        }),
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}
