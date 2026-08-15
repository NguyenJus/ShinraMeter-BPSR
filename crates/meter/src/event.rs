//! Meter-local mirror of the protocol crate's event contract (plan §0.4,
//! §0.6, T1.3).
//!
//! Deviation from the plan: `bpsr-meter` does **not** depend on
//! `bpsr-protocol` (the two crates are being developed concurrently, and
//! decoupling them lets each be implemented and tested in isolation). These
//! types intentionally mirror the shape, field names, and semantics the plan
//! pins for the protocol -> meter boundary (`ProtocolEvent` / `DamageEvent` /
//! `PlayerInfo` / `EnemyHp` / `Class`); the `shinra-bpsr` app crate is
//! responsible for mapping `bpsr_protocol::*` events onto these before
//! calling `Meter::apply`.

/// Entity kind derived from the low 16 bits of a wire `uuid` (plan §0.6):
/// `640` = player, `64` = monster, anything else = unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Player,
    Monster,
    #[default]
    Unknown,
}

/// `uid = uuid >> 16`.
pub fn uid_of(uuid: i64) -> i64 {
    uuid >> 16
}

/// `entity_kind = uuid & 0xFFFF`; `640` = player, `64` = monster.
pub fn kind_of(uuid: i64) -> EntityKind {
    match uuid & 0xFFFF {
        640 => EntityKind::Player,
        64 => EntityKind::Monster,
        _ => EntityKind::Unknown,
    }
}

/// Player class, derived from `ATTR_PROFESSION_ID` / `cur_profession_id`
/// (plan §0.6). Mirrors `bpsr_protocol::pb::Class` exactly.
///
/// `Serialize`/`Deserialize` back the on-disk name cache (issue #12); the
/// derive uses serde's default enum representation (variant name as a JSON
/// string), which is stable across the fields defined here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Class {
    Stormblade,
    FrostMage,
    TwinStriker,
    WindKnight,
    VerdantOracle,
    HeavyGuardian,
    Marksman,
    ShieldKnight,
    BeatPerformer,
    Unknown,
}

impl From<i32> for Class {
    fn from(id: i32) -> Self {
        match id {
            1 => Class::Stormblade,
            2 => Class::FrostMage,
            3 => Class::TwinStriker,
            4 => Class::WindKnight,
            5 => Class::VerdantOracle,
            9 => Class::HeavyGuardian,
            11 => Class::Marksman,
            12 => Class::ShieldKnight,
            13 => Class::BeatPerformer,
            _ => Class::Unknown,
        }
    }
}

impl Class {
    pub fn name(&self) -> &'static str {
        match self {
            Class::Stormblade => "Stormblade",
            Class::FrostMage => "FrostMage",
            Class::TwinStriker => "TwinStriker",
            Class::WindKnight => "WindKnight",
            Class::VerdantOracle => "VerdantOracle",
            Class::HeavyGuardian => "HeavyGuardian",
            Class::Marksman => "Marksman",
            Class::ShieldKnight => "ShieldKnight",
            Class::BeatPerformer => "BeatPerformer",
            Class::Unknown => "Unknown",
        }
    }
}

/// A single damage (or miss/heal) event, already fully resolved by the
/// protocol layer (pet -> summoner attribution, crit/lucky bits, effective
/// value) per plan §0.6. All timestamps are supplied by the caller — no
/// `Instant::now()` inside this pure crate.
#[derive(Clone, Debug, Default, PartialEq)]
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInfo {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power). Mirrors
    /// `bpsr_protocol::PlayerInfo::ability_score` (issue #15).
    pub ability_score: Option<u32>,
    /// Season level. Mirrors `bpsr_protocol::PlayerInfo::season_level`
    /// (issue #15).
    pub season_level: Option<u32>,
    /// Season strength. Mirrors `bpsr_protocol::PlayerInfo::season_strength`
    /// (issue #15).
    pub season_strength: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
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
    /// The dungeon/instance id, mirrors `bpsr_protocol::ProtocolEvent::Scene`
    /// (issue #9 slice 2).
    Scene {
        level_map_id: u32,
    },
    /// `timestamp_ms` is the caller-supplied "now" at detection time (this
    /// event carries no other timing signal of its own) — used to anchor the
    /// post-reset cooldown so it isn't stamped with a stale prior event time.
    ServerChanged {
        timestamp_ms: u64,
    },
}
