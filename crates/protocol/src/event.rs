//! Typed protocol events — the crate's cross-crate contract (plan §T1.3).
//!
//! `ProtocolEvent` is consumed as-is by `bpsr-meter` and the app; freeze the
//! shape here before Phase 2 starts.

use crate::pb::Class;

/// `EEntityType::EntChar`, the entity-type value for a player.
/// (BPSR-ZDPS `BPSR-ZDPSLib/protos/EnumEEntityType.cs:53`; identically
/// bpsr-logs `src-tauri/src/protocol/pb.proto:14`.)
const ENT_CHAR: i64 = 10;
/// `EEntityType::EntMonster`, the entity-type value for a monster.
/// (BPSR-ZDPS `BPSR-ZDPSLib/protos/EnumEEntityType.cs:46`; identically
/// bpsr-logs `src-tauri/src/protocol/pb.proto:13`.)
const ENT_MONSTER: i64 = 1;
/// Bit offset of the entity-type field inside a uuid, and its width (5
/// bits). See `kind_of` for the full layout.
const ENT_TYPE_SHIFT: u32 = 6;
const ENT_TYPE_MASK: i64 = 0x1F;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Player,
    Monster,
    Unknown,
}

/// `uid = uuid >> 16`.
pub fn uid_of(uuid: i64) -> i64 {
    uuid >> 16
}

/// Entity type unpacked from the uuid's bit layout, which BPSR-ZDPS spells
/// out in `Utils.cs:261` (`EntityIdToUuid`):
///
/// ```text
/// uuid = uid << 16 | is_summon << 15 | is_client << 14 | entity_type << 6
/// ```
///
/// so the type is `(uuid >> 6) & 31` (BPSR-ZDPS `Utils.cs:264`,
/// `UuidToEntityType`) and carries the proto enum's own numbering —
/// `EntChar = 10`, `EntMonster = 1`.
///
/// issue #76: this used to match the *whole* low-16 against `640`/`64`,
/// which is only correct while both flag bits are clear. `EntMonster << 6`
/// is 64 and `EntChar << 6` is 640, so the two forms agree on a plain
/// entity — but a summoned monster (bit 15) arrives as `0x8040`, and a
/// client-side entity (bit 14) shifts likewise, and the old form scored
/// both as `Unknown`. `decode.rs` drops `Unknown` entities outright, so
/// those monsters produced no `EnemyHp` at all and could never be named or
/// ranked as the boss. StarResonanceDamageCounter papers over exactly this
/// case by whitelisting the single extra literal `32832` (`0x8040`)
/// alongside `64` (`algo/packet.js:234-237`); masking the flags off, as
/// ZDPS does, handles every combination rather than one.
///
/// Types this meter has no use for (`EntNpc = 2`, `EntPet = 8`,
/// `EntDummy = 11`) stay `Unknown`.
pub fn kind_of(uuid: i64) -> EntityKind {
    match (uuid >> ENT_TYPE_SHIFT) & ENT_TYPE_MASK {
        ENT_CHAR => EntityKind::Player,
        ENT_MONSTER => EntityKind::Monster,
        _ => EntityKind::Unknown,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DamageEvent {
    pub attacker_uid: i64,
    pub attacker_kind: EntityKind,
    pub skill_id: i32,
    pub value: i64,
    pub crit: bool,
    pub lucky: bool,
    pub hp_lessen: i64,
    pub is_miss: bool,
    pub is_heal: bool,
    pub target_uid: i64,
    pub target_kind: EntityKind,
    pub timestamp_ms: u64,
    /// Whether the target died from this hit, sourced from
    /// `pb::SyncDamageInfo` tag 17 (`is_dead`). This is a per-hit flag on the
    /// *victim*, not the attacker — a player entry here means that player
    /// died, not that they scored a kill (issue #49).
    pub is_dead: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power), sourced from `attr_id::FIGHT_POINT`
    /// or `CharBaseInfo.fight_point` — not every packet carries it, so this is
    /// `None` rather than `Some(0)` when absent (issue #15).
    pub ability_score: Option<u32>,
    /// Season level, sourced from `attr_id::SEASON_LEVEL`. `None` rather than
    /// `Some(0)` when absent (issue #15).
    ///
    /// Deliberately decoded but unconsumed above this crate: the UI column
    /// that displayed it was removed in #51, and issue #55 stripped the rest
    /// of the chain (`bpsr_meter::PlayerStats`/`PlayerRow`,
    /// `pipeline::map_event`). `SEASON_LEVEL` is a documented
    /// reverse-engineering finding (`docs/packet-inspection.md`), so it stays
    /// decoded here on purpose — do not "clean up" this field by removing it
    /// from the protocol layer too.
    pub season_level: Option<u32>,
    /// Season strength, sourced from `attr_id::SEASON_STRENGTH`. `None`
    /// rather than `Some(0)` when absent (issue #15).
    pub season_strength: Option<u32>,
    /// Equipped Imagine skill ids (issue #33), sourced from
    /// `attr_id::SKILL_LEVEL_ID_LIST` (`0x74`), in wire order. **Empty ==
    /// absent** — matching `FIGHT_POINT`'s zero-is-absent rule — whether
    /// because the attr wasn't present in this packet or because it decoded
    /// to no ids. This crate stops at raw ids: it never learns what an
    /// Imagine *is* (name/icon classification happens above this crate, in
    /// `crates/app`).
    pub skill_ids: Vec<i32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnemyHp {
    pub uid: i64,
    pub curr_hp: Option<u64>,
    pub max_hp: Option<u64>,
    pub monster_id: Option<u32>,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProtocolEvent {
    Damage(DamageEvent),
    Player(PlayerInfo),
    EnemyHp(EnemyHp),
    /// The dungeon/instance id, decoded from `CharSerialize.scene_data`
    /// (issue #9 slice 2). `SocialData.scene_data`, the other path the
    /// reference implementation reaches this through, is not wired up: the
    /// decoder has no handler for `NotifySocialData`'s opcode today (see
    /// `decode.rs`), and no capture is available here to learn it.
    Scene {
        level_map_id: u32,
    },
    ServerChanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_and_kind_for_player_uuid() {
        let uuid = (12345i64 << 16) | 640;
        assert_eq!(uid_of(uuid), 12345);
        assert_eq!(kind_of(uuid), EntityKind::Player);
    }

    #[test]
    fn uid_and_kind_for_monster_uuid() {
        let uuid = (999i64 << 16) | 64;
        assert_eq!(uid_of(uuid), 999);
        assert_eq!(kind_of(uuid), EntityKind::Monster);
    }

    #[test]
    fn unknown_kind_for_other_low_bits() {
        let uuid = (1i64 << 16) | 7;
        assert_eq!(kind_of(uuid), EntityKind::Unknown);
    }

    /// issue #76: the uuid packs two flags *above* the type field, so
    /// matching the whole low-16 against `64` dropped every summoned
    /// monster on the floor — its `EnemyHp` was never emitted at all, so
    /// the header could never name it.
    #[test]
    fn summoned_monster_uuid_is_still_a_monster() {
        let uuid = (999i64 << 16) | (1 << 15) | (1 << 6);
        // The exact low-16 StarResonanceDamageCounter special-cases as
        // "monster" alongside 64 (`algo/packet.js:234-237`).
        assert_eq!(uuid & 0xFFFF, 0x8040);
        assert_eq!(uid_of(uuid), 999);
        assert_eq!(kind_of(uuid), EntityKind::Monster);
    }

    /// The client flag (bit 14) is the other flag that must not change an
    /// entity's decoded type.
    #[test]
    fn client_flagged_player_uuid_is_still_a_player() {
        let uuid = (12345i64 << 16) | (1 << 14) | (10 << 6);
        assert_eq!(uid_of(uuid), 12345);
        assert_eq!(kind_of(uuid), EntityKind::Player);
    }

    /// Bits 0-5 are unused by the packing, so they must not affect the
    /// decoded type either.
    #[test]
    fn unused_low_bits_do_not_change_the_decoded_kind() {
        let uuid = (7i64 << 16) | (1 << 6) | 0x3F;
        assert_eq!(kind_of(uuid), EntityKind::Monster);
    }

    /// An entity type this meter has no use for (NPC = 2, pet = 8, dummy =
    /// 11) must still decode as `Unknown` rather than being mistaken for a
    /// monster now that the flag bits are masked off.
    #[test]
    fn npc_and_pet_entity_types_stay_unknown() {
        assert_eq!(kind_of((1i64 << 16) | (2 << 6)), EntityKind::Unknown);
        assert_eq!(kind_of((1i64 << 16) | (8 << 6)), EntityKind::Unknown);
        assert_eq!(kind_of((1i64 << 16) | (11 << 6)), EntityKind::Unknown);
    }
}
