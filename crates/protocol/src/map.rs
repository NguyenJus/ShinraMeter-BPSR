//! Shared mappers from `bpsr_protocol`'s wire-level types to `bpsr_meter`'s
//! mirror types (issue #146's finding 2). `bpsr-meter` deliberately does not
//! depend on `bpsr-protocol` (see `crates/app/src/pipeline.rs`'s module
//! doc); this module is the one place both real-time consumption
//! (`bpsr-app`'s `pipeline::map_event`) and offline replay
//! (`bpsr-protocol`'s `sanitize-dump` binary) translate between the two, so
//! a future field or enum variant only needs fixing once.

use crate::event::{EDungeonState, EntityKind, ProtocolEvent};
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
        ProtocolEvent::Damage(d) => meter::ProtocolEvent::Damage(meter::DamageEvent {
            attacker_uid: d.attacker_uid,
            attacker_kind: map_kind(d.attacker_kind),
            skill_id: d.skill_id,
            value: d.value,
            crit: d.crit,
            lucky: d.lucky,
            hp_lessen: d.hp_lessen,
            is_miss: d.is_miss,
            is_heal: d.is_heal,
            target_uid: d.target_uid,
            target_kind: map_kind(d.target_kind),
            timestamp_ms: d.timestamp_ms,
            is_dead: d.is_dead,
        }),
        ProtocolEvent::Player(p) => meter::ProtocolEvent::Player(meter::PlayerInfo {
            uid: p.uid,
            name: p.name,
            class: p.class.map(map_class),
            ability_score: p.ability_score,
            season_strength: p.season_strength,
            imagines,
            imagine_tiers,
        }),
        ProtocolEvent::EnemyHp(e) => meter::ProtocolEvent::EnemyHp(meter::EnemyHp {
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
    }
}
