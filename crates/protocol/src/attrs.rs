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

/// protobuf varint → `i64`; `None` on empty/malformed input.
pub fn decode_varint_i64(raw: &[u8]) -> Option<i64> {
    let mut cursor = Cursor::new(raw);
    prost::encoding::decode_varint(&mut cursor)
        .ok()
        .map(|v| v as i64)
}

/// protobuf varint → `i32`; `None` on empty/malformed input.
pub fn decode_varint_i32(raw: &[u8]) -> Option<i32> {
    decode_varint_i64(raw).map(|v| v as i32)
}

/// Drops the stray leading byte (a length/tag byte the server prepends),
/// then UTF-8 decodes the remainder. Invalid UTF-8 → `None`.
pub fn decode_name(raw: &[u8]) -> Option<String> {
    let rest = raw.get(1..)?;
    std::str::from_utf8(rest).ok().map(str::to_string)
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
                if let Some(v) = decode_varint_i64(&attr.raw_data) {
                    curr_hp = Some(v.max(0) as u64);
                }
            }
            attr_id::MAX_HP => {
                if let Some(v) = decode_varint_i64(&attr.raw_data) {
                    max_hp = Some(v.max(0) as u64);
                }
            }
            attr_id::MONSTER_ID => {
                if let Some(v) = decode_varint_i32(&attr.raw_data) {
                    monster_id = Some(v.max(0) as u32);
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

    #[test]
    fn unknown_attr_id_ignored() {
        let mut raw = Vec::new();
        prost::encoding::encode_varint(999u64, &mut raw);
        let attrs = vec![pb::Attr { id: 0x9999, raw_data: raw }];
        let info = player_info_from_attrs(1, &attrs);
        assert_eq!(info.name, None);
        assert_eq!(info.class, None);
    }
}
