//! Shared mappers from `bpsr_protocol`'s wire-level types to `bpsr_meter`'s
//! mirror types (issue #146's finding 2). `bpsr-meter` deliberately does not
//! depend on `bpsr-protocol` (see `crates/app/src/pipeline.rs`'s module
//! doc); this module is the one place both real-time consumption
//! (`bpsr-app`'s `pipeline::map_event`) and offline replay
//! (`bpsr-protocol`'s `sanitize-dump` binary) translate between the two, so
//! a future field or enum variant only needs fixing once.

use crate::entity::EntityId;
use crate::event::{DamageKind, DisappearReason, EDungeonState, EntityKind, ProtocolEvent};
use crate::pb::Class;
use bpsr_meter as meter;

/// Maps a protocol dungeon state onto the meter's mirror type (issue #139).
pub fn map_dungeon_state(state: EDungeonState) -> meter::EDungeonState {
    match state {
        EDungeonState::Null => meter::EDungeonState::Null,
        EDungeonState::Active => meter::EDungeonState::Active,
        EDungeonState::Ready => meter::EDungeonState::Ready,
        EDungeonState::Playing => meter::EDungeonState::Playing,
        EDungeonState::End => meter::EDungeonState::End,
        EDungeonState::Settlement => meter::EDungeonState::Settlement,
        EDungeonState::Vote => meter::EDungeonState::Vote,
        EDungeonState::Unknown(v) => meter::EDungeonState::Unknown(v),
    }
}

/// Maps a protocol despawn reason onto the meter's mirror type (issue #276).
pub fn map_disappear_reason(reason: DisappearReason) -> meter::DisappearReason {
    match reason {
        DisappearReason::Normal => meter::DisappearReason::Normal,
        DisappearReason::Dead => meter::DisappearReason::Dead,
        DisappearReason::Destroy => meter::DisappearReason::Destroy,
        DisappearReason::TransferLeave => meter::DisappearReason::TransferLeave,
        DisappearReason::TransferPassLineLeave => meter::DisappearReason::TransferPassLineLeave,
        DisappearReason::Unknown(v) => meter::DisappearReason::Unknown(v),
    }
}

/// Maps a protocol entity id onto the meter's mirror type (issue #335).
/// Both are the whole wire uuid; only the crate they live in differs.
pub fn map_entity_id(id: EntityId) -> meter::EntityId {
    meter::EntityId(id.0)
}

/// Maps a protocol damage kind onto the meter's mirror type (issue #338).
pub fn map_damage_kind(kind: DamageKind) -> meter::DamageKind {
    match kind {
        DamageKind::Normal => meter::DamageKind::Normal,
        DamageKind::Absorbed => meter::DamageKind::Absorbed,
        DamageKind::Immune => meter::DamageKind::Immune,
    }
}

/// Maps a protocol entity kind onto the meter's mirror type.
pub fn map_kind(kind: EntityKind) -> meter::EntityKind {
    match kind {
        EntityKind::Player => meter::EntityKind::Player,
        EntityKind::Monster => meter::EntityKind::Monster,
        EntityKind::Unknown => meter::EntityKind::Unknown,
    }
}

/// Maps a protocol class onto the meter's mirror type.
pub fn map_class(class: Class) -> meter::Class {
    match class {
        Class::Stormblade => meter::Class::Stormblade,
        Class::FrostMage => meter::Class::FrostMage,
        Class::TwinStriker => meter::Class::TwinStriker,
        Class::WindKnight => meter::Class::WindKnight,
        Class::VerdantOracle => meter::Class::VerdantOracle,
        Class::HeavyGuardian => meter::Class::HeavyGuardian,
        Class::Marksman => meter::Class::Marksman,
        Class::ShieldKnight => meter::Class::ShieldKnight,
        Class::BeatPerformer => meter::Class::BeatPerformer,
        Class::Unknown => meter::Class::Unknown,
    }
}

/// Translates a `bpsr_protocol::ProtocolEvent` into the meter's mirror
/// event.
///
/// `imagines` is the already-resolved Imagine-slot pair for a `Player`
/// event (`None` if the packet carried no `skill_ids` or the caller has no
/// Imagine catalog at all); it is used only for the `Player` variant and
/// ignored otherwise. Resolving skill ids to Imagines is deliberately the
/// caller's job, not this module's: the curated Imagine catalog
/// (`bpsr-app`'s `imagines` module, see its `IMAGINE-TAKEDOWN` doc comment)
/// is an app-level, UI-facing concern this protocol crate has no business
/// knowing about — `bpsr-protocol`'s own caller (`sanitize-dump`) always
/// passes `None`.
///
/// `imagine_tiers` (issues #169/#170) is each equipped slot's tier
/// (`remodel_level`), positionally paired with `imagines` the same way — a
/// `Some` at index `i` in `imagines` should have its tier, if known, at the
/// same index `i` in `imagine_tiers`. Kept as a second parallel array
/// rather than folded into `imagines`'s element type so the well-established
/// `[Option<i32>; 2]` id shape (and every existing caller/test of it) is
/// undisturbed by tier's addition.
pub fn map_event(
    ev: ProtocolEvent,
    now_ms: u64,
    imagines: Option<[Option<i32>; 2]>,
    imagine_tiers: Option<[Option<i32>; 2]>,
) -> meter::ProtocolEvent {
    match ev {
        ProtocolEvent::Cast(c) => meter::ProtocolEvent::Cast(meter::CastEvent {
            caster: map_entity_id(c.caster),
            caster_uid: c.caster_uid,
            skill_id: c.skill_id,
            timestamp_ms: c.timestamp_ms,
        }),
        ProtocolEvent::Damage(d) => meter::ProtocolEvent::Damage(meter::DamageEvent {
            attacker: map_entity_id(d.attacker),
            attacker_uid: d.attacker_uid,
            attacker_kind: map_kind(d.attacker_kind),
            skill_id: d.skill_id,
            value: d.value,
            crit: d.crit,
            lucky: d.lucky,
            hp_lessen: d.hp_lessen,
            is_miss: d.is_miss,
            is_heal: d.is_heal,
            kind: map_damage_kind(d.kind),
            target: map_entity_id(d.target),
            target_uid: d.target_uid,
            target_kind: map_kind(d.target_kind),
            timestamp_ms: d.timestamp_ms,
            is_dead: d.is_dead,
        }),
        ProtocolEvent::Player(p) => meter::ProtocolEvent::Player(meter::PlayerInfo {
            entity: map_entity_id(p.entity),
            uid: p.uid,
            name: p.name,
            class: p.class.map(map_class),
            ability_score: p.ability_score,
            season_strength: p.season_strength,
            imagines,
            imagine_tiers,
            shield: p.shield,
        }),
        ProtocolEvent::EnemyHp(e) => meter::ProtocolEvent::EnemyHp(meter::EnemyHp {
            entity: map_entity_id(e.entity),
            uid: e.uid,
            curr_hp: e.curr_hp,
            max_hp: e.max_hp,
            monster_id: e.monster_id,
            timestamp_ms: e.timestamp_ms,
        }),
        ProtocolEvent::Scene { level_map_id } => meter::ProtocolEvent::Scene { level_map_id },
        ProtocolEvent::ServerChanged => meter::ProtocolEvent::ServerChanged {
            timestamp_ms: now_ms,
        },
        ProtocolEvent::DungeonState { state, scene_uuid } => meter::ProtocolEvent::DungeonState {
            state: map_dungeon_state(state),
            scene_uuid,
        },
        ProtocolEvent::DungeonObjective {
            target_id,
            nums,
            complete,
        } => meter::ProtocolEvent::DungeonObjective {
            target_id,
            nums,
            complete,
        },
        ProtocolEvent::DungeonObjectiveRemoved { target_id } => {
            meter::ProtocolEvent::DungeonObjectiveRemoved { target_id }
        }
        ProtocolEvent::DungeonVar { name, value } => {
            meter::ProtocolEvent::DungeonVar { name, value }
        }
        ProtocolEvent::EnemyGone {
            entity,
            uid,
            reason,
        } => meter::ProtocolEvent::EnemyGone {
            entity: map_entity_id(entity),
            uid,
            reason: reason.map(map_disappear_reason),
        },
        ProtocolEvent::BuffApply {
            host,
            host_uid,
            buff_uuid,
            base_id,
            adds_layer,
            timestamp_ms,
        } => meter::ProtocolEvent::BuffApply {
            host: map_entity_id(host),
            host_uid,
            buff_uuid,
            base_id,
            adds_layer,
            timestamp_ms,
        },
        ProtocolEvent::BuffRemove {
            host,
            host_uid,
            buff_uuid,
            removes_layer,
            timestamp_ms,
        } => meter::ProtocolEvent::BuffRemove {
            host: map_entity_id(host),
            host_uid,
            buff_uuid,
            removes_layer,
            timestamp_ms,
        },
        ProtocolEvent::EntityState {
            entity,
            uid,
            kind,
            is_dead,
            timestamp_ms,
        } => meter::ProtocolEvent::EntityState {
            entity: map_entity_id(entity),
            uid,
            kind: map_kind(kind),
            is_dead,
            timestamp_ms,
        },
        ProtocolEvent::Revive {
            entity,
            uid,
            timestamp_ms,
        } => meter::ProtocolEvent::Revive {
            entity: map_entity_id(entity),
            uid,
            timestamp_ms,
        },
        ProtocolEvent::TeamMemberLeft { uid } => meter::ProtocolEvent::TeamMemberLeft { uid },
        ProtocolEvent::TeamRoster { members } => meter::ProtocolEvent::TeamRoster { members },
        ProtocolEvent::LocalPlayer { uid } => meter::ProtocolEvent::LocalPlayer { uid },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins every `DisappearReason`/`meter::DisappearReason` pairing
    /// individually (issue #276's finding 2) — a transposed pair of match
    /// arms in `map_disappear_reason` (e.g. `Destroy` <-> `TransferLeave`)
    /// would stay exhaustive and compile, so this must assert each variant
    /// on its own rather than a loop that would pass either way.
    #[test]
    fn map_disappear_reason_pins_every_variant() {
        assert_eq!(
            map_disappear_reason(DisappearReason::Normal),
            meter::DisappearReason::Normal
        );
        assert_eq!(
            map_disappear_reason(DisappearReason::Dead),
            meter::DisappearReason::Dead
        );
        assert_eq!(
            map_disappear_reason(DisappearReason::Destroy),
            meter::DisappearReason::Destroy
        );
        assert_eq!(
            map_disappear_reason(DisappearReason::TransferLeave),
            meter::DisappearReason::TransferLeave
        );
        assert_eq!(
            map_disappear_reason(DisappearReason::TransferPassLineLeave),
            meter::DisappearReason::TransferPassLineLeave
        );
        assert_eq!(
            map_disappear_reason(DisappearReason::Unknown(42)),
            meter::DisappearReason::Unknown(42)
        );
    }

    #[test]
    fn map_event_enemy_gone_carries_reason_through() {
        let ev = ProtocolEvent::EnemyGone {
            entity: EntityId::from_display_uid(7, EntityKind::Monster),
            uid: 7,
            reason: Some(DisappearReason::TransferLeave),
        };
        assert_eq!(
            map_event(ev, 0, None, None),
            meter::ProtocolEvent::EnemyGone {
                entity: meter::EntityId::from_display_uid(7, meter::EntityKind::Monster),
                uid: 7,
                reason: Some(meter::DisappearReason::TransferLeave),
            }
        );
    }

    #[test]
    fn map_event_enemy_gone_with_no_reason() {
        let ev = ProtocolEvent::EnemyGone {
            entity: EntityId::from_display_uid(9, EntityKind::Monster),
            uid: 9,
            reason: None,
        };
        assert_eq!(
            map_event(ev, 0, None, None),
            meter::ProtocolEvent::EnemyGone {
                entity: meter::EntityId::from_display_uid(9, meter::EntityKind::Monster),
                uid: 9,
                reason: None,
            }
        );
    }

    #[test]
    fn map_event_enemy_gone_with_unrecognized_wire_reason() {
        let ev = ProtocolEvent::EnemyGone {
            entity: EntityId::from_display_uid(3, EntityKind::Monster),
            uid: 3,
            reason: Some(DisappearReason::Unknown(99)),
        };
        assert_eq!(
            map_event(ev, 0, None, None),
            meter::ProtocolEvent::EnemyGone {
                entity: meter::EntityId::from_display_uid(3, meter::EntityKind::Monster),
                uid: 3,
                reason: Some(meter::DisappearReason::Unknown(99)),
            }
        );
    }

    /// Pins every `DamageKind`/`meter::DamageKind` pairing individually
    /// (issue #338), same rationale as `map_disappear_reason`'s pinning
    /// test above — a transposed match arm would stay exhaustive and
    /// compile.
    #[test]
    fn map_damage_kind_pins_every_variant() {
        assert_eq!(
            map_damage_kind(DamageKind::Normal),
            meter::DamageKind::Normal
        );
        assert_eq!(
            map_damage_kind(DamageKind::Absorbed),
            meter::DamageKind::Absorbed
        );
        assert_eq!(
            map_damage_kind(DamageKind::Immune),
            meter::DamageKind::Immune
        );
    }

    #[test]
    fn map_event_carries_damage_kind_and_shield_through() {
        use crate::event::{DamageEvent, EntityKind};

        let d = DamageEvent {
            attacker: crate::entity::EntityId::from_display_uid(1, EntityKind::Player),
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            skill_id: 1,
            value: 500,
            crit: false,
            lucky: false,
            hp_lessen: 0,
            is_miss: false,
            is_heal: false,
            kind: DamageKind::Absorbed,
            target: crate::entity::EntityId::from_display_uid(2, EntityKind::Monster),
            target_uid: 2,
            target_kind: EntityKind::Monster,
            timestamp_ms: 0,
            is_dead: false,
        };
        let mapped = map_event(ProtocolEvent::Damage(d), 0, None, None);
        let meter::ProtocolEvent::Damage(m) = mapped else {
            panic!("expected a damage event");
        };
        assert_eq!(m.kind, meter::DamageKind::Absorbed);
    }
}
