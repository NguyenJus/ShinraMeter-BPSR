//! Attribute id constants and varint/name decoding (plan §0.6).
//!
//! Every function here is non-panicking: an empty `raw_data`, `id == 0`, or a
//! malformed varint/utf8 payload is skipped rather than propagated as an
//! error.

use std::io::Cursor;

use crate::event::{EnemyHp, PlayerInfo};
use crate::pb::{self, Class};

pub mod attr_id {
    pub const NAME: i32 = 0x01;
    pub const MONSTER_ID: i32 = 0x0A;
    pub const HP: i32 = 0x2C2E;
    pub const MAX_HP: i32 = 0x2C38;
    pub const PROFESSION_ID: i32 = 0xDC;
    pub const FIGHT_POINT: i32 = 0x272E;
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

/// Builds a `PlayerInfo` from an entity's `Attr` list, reading `NAME` and
/// `PROFESSION_ID`. Unknown ids, empty `raw_data`, and `id == 0` are skipped.
pub fn player_info_from_attrs(uid: i64, attrs: &[pb::Attr]) -> PlayerInfo {
    let mut name = None;
    let mut class = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        match attr.id {
            attr_id::NAME => {
                if let Some(n) = decode_name(&attr.raw_data) {
                    name = Some(n);
                }
            }
            attr_id::PROFESSION_ID => {
                if let Some(id) = decode_varint_i32(&attr.raw_data) {
                    class = Some(Class::from(id));
                }
            }
            _ => {}
        }
    }
    PlayerInfo { uid, name, class }
}

/// Builds an `EnemyHp` from an entity's `Attr` list, reading `HP`, `MAX_HP`,
/// and `MONSTER_ID`. Unknown ids, empty `raw_data`, and `id == 0` are
/// skipped.
pub fn enemy_hp_from_attrs(uid: i64, attrs: &[pb::Attr], now_ms: u64) -> EnemyHp {
    let mut curr_hp = None;
    let mut max_hp = None;
    let mut monster_id = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
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
                if let Some(v) =
                    decode_varint_u64(&attr.raw_data).and_then(|v| u32::try_from(v).ok())
                {
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
        let hp = enemy_hp_from_attrs(1, &attrs, 0);
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
        let hp = enemy_hp_from_attrs(1, &attrs, 0);
        assert_eq!(hp.curr_hp, Some(u64::MAX));
    }

    #[test]
    fn out_of_range_profession_id_yields_no_class() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(0x1_0000_0001),
        }];
        assert_eq!(player_info_from_attrs(1, &attrs).class, None);
    }

    #[test]
    fn out_of_range_monster_id_is_rejected() {
        let attrs = vec![pb::Attr {
            id: attr_id::MONSTER_ID,
            raw_data: varint(u64::from(u32::MAX) + 1),
        }];
        assert_eq!(enemy_hp_from_attrs(1, &attrs, 0).monster_id, None);
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
    fn unknown_attr_id_ignored() {
        let mut raw = Vec::new();
        prost::encoding::encode_varint(999u64, &mut raw);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw,
        }];
        let info = player_info_from_attrs(1, &attrs);
        assert_eq!(info.name, None);
        assert_eq!(info.class, None);
    }
}
