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

/// Mirrors `bpsr_protocol::event::CastEvent` (issue #245): one skill
/// activation, with no amount attached. See that type for the wire source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CastEvent {
    pub caster_uid: i64,
    pub skill_id: i32,
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
    /// Season strength. Mirrors `bpsr_protocol::PlayerInfo::season_strength`
    /// (issue #15).
    pub season_strength: Option<u32>,
    /// The two equipped Imagines, as opaque skill ids resolved to display
    /// data (name/icon) only by `crates/app/src/imagines.rs` (issue #33) —
    /// `bpsr-meter` never learns what an Imagine is.
    ///
    /// `None` means no `0x74` (`SKILL_LEVEL_ID_LIST`) packet has been seen
    /// yet for this player, so a cached pair (if any) must not be clobbered.
    /// `Some([None, None])` means a packet *was* seen and this player has no
    /// known Imagines — this does overwrite, the same "live wins" rule
    /// `name_upsert` already applies to `ability_score`/`season_strength`.
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    pub imagines: Option<[Option<i32>; 2]>,
    /// Each equipped slot's tier (`remodel_level`, issues #169/#170;
    /// BPSR-ZDPS calls it `Tier`), positionally paired with `imagines` —
    /// index `i` here is `imagines[i]`'s tier, if known. Same `None`/
    /// `Some([None, None])` semantics as `imagines`: `None` means no `0x74`
    /// packet has been seen yet, so a cached pair must not be clobbered.
    /// Kept as a separate field rather than folded into `imagines`'s
    /// element type to leave that already-established `[Option<i32>; 2]`
    /// id shape (and its many existing callers/tests) undisturbed.
    pub imagine_tiers: Option<[Option<i32>; 2]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnemyHp {
    pub uid: i64,
    pub curr_hp: Option<u64>,
    pub max_hp: Option<u64>,
    pub monster_id: Option<u32>,
    pub timestamp_ms: u64,
}

/// Mirrors `bpsr_protocol::event::EDungeonState` exactly (issue #139): see
/// that type's doc comment for the wire source and why `Unknown` is a
/// distinct variant rather than folded into `Null`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EDungeonState {
    Null,
    Active,
    Ready,
    Playing,
    End,
    Settlement,
    Vote,
    Unknown(i32),
}

/// Mirrors `bpsr_protocol::event::DisappearReason` exactly (issue #276),
/// which in turn mirrors `bpsr_protocol::pb::EDisappearType` — see those
/// types' doc comments for the reference sourcing and the live-capture
/// evidence behind each variant.
///
/// Only [`DisappearReason::Dead`] is a death; everything else is an
/// eviction, a zone-out or ordinary streaming churn. `Unknown` is explicit
/// for the same reason [`EDungeonState`]'s is, and is treated as "no usable
/// reason" — see `Meter::apply_enemy_gone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DisappearReason {
    Normal,
    Dead,
    Destroy,
    TransferLeave,
    TransferPassLineLeave,
    Unknown(i32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProtocolEvent {
    Damage(DamageEvent),
    /// Mirrors `bpsr_protocol::ProtocolEvent::Cast` (issue #245). See
    /// `Meter::apply_cast` for what the meter does with it — notably, a
    /// cast never starts the fight clock, opens a row, or counts as
    /// evidence of a revive.
    Cast(CastEvent),
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
    /// Mirrors `bpsr_protocol::ProtocolEvent::DungeonState` (issue #139).
    /// `Meter::apply` acts on this — see `Meter::apply_dungeon_state` for
    /// the full dungeon-state / objective behaviour.
    DungeonState {
        state: EDungeonState,
        scene_uuid: Option<u32>,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::DungeonObjective` (issue
    /// #139). See `Meter::apply_dungeon_objective`.
    DungeonObjective {
        target_id: i32,
        nums: Option<i32>,
        complete: Option<bool>,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::DungeonObjectiveRemoved`
    /// (issue #139). See `Meter::apply_dungeon_objective_removed`.
    DungeonObjectiveRemoved {
        target_id: i32,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::DungeonVar` (issue #139).
    /// `Meter::apply` acts only on `name == "IsFinishTarget"`; every other
    /// var is decoded and ignored.
    DungeonVar {
        name: String,
        value: i32,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::EnemyGone` (issue #215): a
    /// monster left the client's area of interest.
    ///
    /// `reason` is the server's own statement of why (issue #276), from
    /// `pb::DisappearEntity`'s optional tag 2. `None` means the packet
    /// carried no tag 2 at all — 382 of 851 observed disappear entries —
    /// not that nothing happened.
    ///
    /// A despawn is still not a death by itself: only
    /// [`DisappearReason::Dead`] says the enemy died, and a `None` falls
    /// back to the HP/engagement heuristic. See `Meter::apply_enemy_gone`.
    EnemyGone {
        uid: i64,
        reason: Option<DisappearReason>,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::BuffApply` (issue #267). See
    /// `Meter::apply_buff_apply` for what the meter does with it.
    BuffApply {
        host_uid: i64,
        buff_uuid: i32,
        base_id: Option<i32>,
        adds_layer: bool,
        timestamp_ms: u64,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::BuffRemove` (issue #267). See
    /// `Meter::apply_buff_remove`.
    BuffRemove {
        host_uid: i64,
        buff_uuid: i32,
        removes_layer: bool,
        timestamp_ms: u64,
    },
    /// Mirrors `bpsr_protocol::ProtocolEvent::LocalPlayer` (issue #344):
    /// the local player's own uid, decoded from
    /// `SyncContainerData.v_data.char_id`. Session-scoped, not
    /// fight-scoped — `Meter::apply` stores it outside anything `reset`
    /// touches, and clears it only on `ServerChanged` (a new server
    /// session hands out fresh uids). See `Meter::local_uid`.
    LocalPlayer {
        uid: i64,
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
