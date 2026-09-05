#![forbid(unsafe_code)]

pub mod encounter;
pub mod event;
pub mod fight;
pub mod names_cache;
pub mod phase;
pub mod reset;
pub mod sim;
pub mod stats;
pub mod tables;

pub use encounter::{Meter, skill_row_from_stats};
pub use event::{
    CastEvent, Class, DamageEvent, DamageKind, DisappearReason, EDungeonState, EnemyHp, EntityId,
    EntityKind, PlayerInfo, ProtocolEvent, Role,
};
pub use fight::{FightConfig, FightEndCause, FightState, HoldKind, Lifecycle};
pub use reset::{EnemyState, ResetConfig, ResetReason, check_hp_rollback};
pub use stats::{EncounterInfo, PlayerRow, PlayerStats, SkillRow, SkillStats, Snapshot};
