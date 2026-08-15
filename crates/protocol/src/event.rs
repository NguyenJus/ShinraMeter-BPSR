//! Typed protocol events — the crate's cross-crate contract (plan §T1.3).
//!
//! `ProtocolEvent` is consumed as-is by `bpsr-meter` and the app; freeze the
//! shape here before Phase 2 starts.

use crate::pb::Class;

/// Uuid low-16 value identifying a player entity (plan §0.6).
const KIND_PLAYER: i64 = 640;
/// Uuid low-16 value identifying a monster entity (plan §0.6).
const KIND_MONSTER: i64 = 64;

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

/// `entity_kind = uuid & 0xFFFF`; `640` = player, `64` = monster, else
/// unknown. (Differs from the proto enum's own numbering 10/1 — this is the
/// wire uuid low-16, not `EEntityType`.)
pub fn kind_of(uuid: i64) -> EntityKind {
    match uuid & 0xFFFF {
        KIND_PLAYER => EntityKind::Player,
        KIND_MONSTER => EntityKind::Monster,
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
    pub season_level: Option<u32>,
    /// Season strength, sourced from `attr_id::SEASON_STRENGTH`. `None`
    /// rather than `Some(0)` when absent (issue #15).
    pub season_strength: Option<u32>,
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
}
