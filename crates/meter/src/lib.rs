#![forbid(unsafe_code)]

pub mod encounter;
pub mod event;
pub mod names_cache;
pub mod reset;
pub mod sim;
pub mod stats;
pub mod tables;

pub use encounter::Meter;
pub use event::{Class, DamageEvent, EnemyHp, EntityKind, PlayerInfo, ProtocolEvent, Role};
pub use reset::{EnemyState, ResetConfig, ResetReason, check_hp_rollback};
pub use stats::{EncounterInfo, PlayerRow, PlayerStats, Snapshot};
