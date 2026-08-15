//! Meter-local mirror of the protocol crate's event contract (plan §0.4,
//! §0.6, T1.3).
//!
//! Deviation from the plan: `bpsr-meter` does **not** depend on
//! `bpsr-protocol` (the two crates are being developed concurrently, and
//! decoupling them lets each be implemented and tested in isolation). These
//! types intentionally mirror the shape, field names, and semantics the plan
//! pins for the protocol -> meter boundary (`ProtocolEvent` / `DamageEvent` /
//! `PlayerInfo` / `EnemyHp` / `Class`); the `ShinraMeter-BPSR` app crate is
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

    /// Role classification for the row share-bar color (issue #44).
    /// `Class::Unknown` has no role (`None`) — the meter couldn't determine
    /// what this player is, so it can't say what role they fill either.
    ///
    /// Mapping source: `Blue-Protocol-Source/BPSR-ZDPS` (already credited in
    /// this project's README and `THIRD_PARTY_NOTICES.md`),
    /// `DataTypes/Enums/Professions.cs` (`enum ERoleType { None=0, Tank=1,
    /// Healer=2, DPS=3 }`) and `DataTypes/Professions.cs::GetRoleFromBaseProfessionId`:
    /// `HeavyGuardian`/`ShieldKnight` -> Tank, `VerdantOracle`/`BeatPerformer`
    /// -> Healer, `Stormblade`/`FrostMage`/`TwinStriker`/`WindKnight`/`Marksman`
    /// -> Damage.
    ///
    /// Deliberately an exhaustive match with **no wildcard arm**: adding a
    /// future `Class` variant without also updating this match is a compile
    /// error, not a silent fall-through into whichever arm happens to be
    /// listed last.
    pub fn role(&self) -> Option<Role> {
        match self {
            Class::HeavyGuardian | Class::ShieldKnight => Some(Role::Tank),
            Class::VerdantOracle | Class::BeatPerformer => Some(Role::Healer),
            Class::Stormblade
            | Class::FrostMage
            | Class::TwinStriker
            | Class::WindKnight
            | Class::Marksman => Some(Role::Damage),
            Class::Unknown => None,
        }
    }
}

/// Combat role a `Class` fills (issue #44): drives the row share-bar's hue
/// in the UI (`crates/app/src/ui.rs`). See `Class::role` for the mapping and
/// its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Tank,
    Healer,
    Damage,
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
    /// Whether `target_uid` died from this hit. Mirrors
    /// `bpsr_protocol::DamageEvent::is_dead`, sourced from
    /// `pb::SyncDamageInfo` tag 17 — a victim-side signal, not an
    /// attacker-side kill count (issue #49).
    pub is_dead: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInfo {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power). Mirrors
    /// `bpsr_protocol::PlayerInfo::ability_score` (issue #15).
    pub ability_score: Option<u32>,
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- Class::role (issue #44) ------------------------------------------
    //
    // Every variant asserted explicitly (rather than looped over an array of
    // pairs) so the mapping is pinned: a reviewer can see the whole table at
    // a glance, and a copy/paste mistake in a table would be just as easy to
    // miss as one in the `match` it's meant to check.

    #[test]
    fn heavy_guardian_is_tank() {
        assert_eq!(Class::HeavyGuardian.role(), Some(Role::Tank));
    }

    #[test]
    fn shield_knight_is_tank() {
        assert_eq!(Class::ShieldKnight.role(), Some(Role::Tank));
    }

    #[test]
    fn verdant_oracle_is_healer() {
        assert_eq!(Class::VerdantOracle.role(), Some(Role::Healer));
    }

    #[test]
    fn beat_performer_is_healer() {
        assert_eq!(Class::BeatPerformer.role(), Some(Role::Healer));
    }

    #[test]
    fn stormblade_is_damage() {
        assert_eq!(Class::Stormblade.role(), Some(Role::Damage));
    }

    #[test]
    fn frost_mage_is_damage() {
        assert_eq!(Class::FrostMage.role(), Some(Role::Damage));
    }

    #[test]
    fn twin_striker_is_damage() {
        assert_eq!(Class::TwinStriker.role(), Some(Role::Damage));
    }

    #[test]
    fn wind_knight_is_damage() {
        assert_eq!(Class::WindKnight.role(), Some(Role::Damage));
    }

    #[test]
    fn marksman_is_damage() {
        assert_eq!(Class::Marksman.role(), Some(Role::Damage));
    }

    #[test]
    fn unknown_has_no_role() {
        assert_eq!(Class::Unknown.role(), None);
    }

    // -- profession-id round trip (issue #44) ------------------------------
    //
    // Exercises `Class::from(id).role()` for every id `From<i32>` maps to a
    // named class, so a future edit to either the id table or the role
    // mapping that silently breaks the pairing gets caught here too, not
    // just in the two tables above independently.

    #[test]
    fn profession_id_round_trip_matches_documented_role() {
        let cases: [(i32, Option<Role>); 10] = [
            (1, Some(Role::Damage)),  // Stormblade
            (2, Some(Role::Damage)),  // FrostMage
            (3, Some(Role::Damage)),  // TwinStriker
            (4, Some(Role::Damage)),  // WindKnight
            (5, Some(Role::Healer)),  // VerdantOracle
            (9, Some(Role::Tank)),    // HeavyGuardian
            (11, Some(Role::Damage)), // Marksman
            (12, Some(Role::Tank)),   // ShieldKnight
            (13, Some(Role::Healer)), // BeatPerformer
            (999, None),              // unmapped id -> Class::Unknown
        ];
        for (id, expected) in cases {
            assert_eq!(
                Class::from(id).role(),
                expected,
                "profession id {id} round-tripped to the wrong role"
            );
        }
    }
}
