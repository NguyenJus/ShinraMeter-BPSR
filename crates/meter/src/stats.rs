//! Per-player stats and the UI-facing snapshot read model (plan §T2.1).

use crate::event::Class;

#[derive(Debug, Clone)]
pub struct PlayerStats {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power); `None` until a packet carrying
    /// it (attrs `FIGHT_POINT`, or `SyncContainerData.fight_point`) has been
    /// seen for this player (issue #15).
    pub ability_score: Option<u32>,
    /// Season strength; `None` until a packet carrying it (attrs
    /// `SEASON_STRENGTH`) has been seen for this player (issue #15).
    pub season_strength: Option<u32>,
    /// The two equipped Imagines, as opaque skill ids; `None` until a `0x74`
    /// packet has been seen for this player, `Some([None, None])` once one
    /// has been seen but no known Imagine was found (issue #33). See
    /// `event::PlayerInfo::imagines` for the full merge-rule doc.
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    pub imagines: Option<[Option<i32>; 2]>,
    pub total_damage: i64,
    pub hits: u64,
    pub crit_hits: u64,
    pub crit_damage: i64,
    pub lucky_hits: u64,
    pub lucky_damage: i64,
    /// Times this player died this encounter, per `DamageEvent::is_dead`
    /// (issue #49). This counts the *victim*, not the attacker — see
    /// `Meter::apply_damage`.
    pub deaths: u32,
    /// Timestamp (event clock, not wall time) of the last death counted for
    /// this player, used to debounce a retransmitted/duplicated delta packet
    /// from double-counting a single death (issue #49). `pub(crate)`, not
    /// part of the display DTO (`PlayerRow`) — see `DEATH_DEBOUNCE_MS`.
    pub(crate) last_death_ms: Option<u64>,
}

impl PlayerStats {
    pub fn new(uid: i64) -> Self {
        Self {
            uid,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: None,
            total_damage: 0,
            hits: 0,
            crit_hits: 0,
            crit_damage: 0,
            lucky_hits: 0,
            lucky_damage: 0,
            deaths: 0,
            last_death_ms: None,
        }
    }

    pub fn crit_pct(&self) -> f32 {
        if self.hits == 0 {
            0.0
        } else {
            self.crit_hits as f32 / self.hits as f32 * 100.0
        }
    }

    pub fn lucky_pct(&self) -> f32 {
        if self.hits == 0 {
            0.0
        } else {
            self.lucky_hits as f32 / self.hits as f32 * 100.0
        }
    }
}

/// The UI's read model for one row of the meter table.
#[derive(Debug, Clone)]
pub struct PlayerRow {
    pub uid: i64,
    pub name: String,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power); `None` when no packet carrying
    /// it has been seen for this player yet (issue #15).
    pub ability_score: Option<u32>,
    /// Season strength; `None` when no packet carrying it has been seen for
    /// this player yet (issue #15).
    pub season_strength: Option<u32>,
    /// The two equipped Imagine slots, as opaque skill ids resolved to
    /// display data only by `crates/app/src/imagines.rs` (issue #33). Always
    /// exactly two slots — `None` per-slot means empty/unknown, never a
    /// missing packet (that distinction lives on `PlayerStats::imagines`).
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    pub imagines: [Option<i32>; 2],
    pub damage: i64,
    pub dps: f64,
    pub share_pct: f32,
    pub crit_pct: f32,
    pub lucky_pct: f32,
    pub hits: u64,
    /// Times this player died this encounter (issue #49). See
    /// `PlayerStats::deaths`.
    pub deaths: u32,
}

/// What the meter believes is being fought, as far as the packet stream reveals
/// it. Every field is independently unknown-able: a scene id can arrive before
/// any boss is identified, and a boss can be identified in an unnamed scene.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncounterInfo {
    pub boss_monster_id: Option<u32>,
    pub boss_name: Option<&'static str>,
    /// Whether `boss_monster_id` is a recognized boss (issue #42), i.e.
    /// whether the header should ever show a name for it. Kept separate from
    /// `boss_name.is_some()` because a boss can be in `tables::BOSS_MONSTER_IDS`
    /// without a resolved name (the two vendored lists aren't guaranteed to
    /// agree) — `is_boss` is the single source of truth for the display
    /// gate, `boss_name` for the text.
    pub is_boss: bool,
    pub scene_id: Option<u32>,
    pub scene_name: Option<&'static str>,
}

/// Cheap, immutable snapshot of the current encounter, sorted by damage
/// descending.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub duration_ms: u64,
    pub total_damage: i64,
    /// Aggregate DPS over the same window used for each row's `dps` (see
    /// `Meter::snapshot`), not `duration_ms` — keeping the header and rows on
    /// separate denominators lets them diverge (e.g. a huge spike on the
    /// first tick, or the header decaying while idle).
    pub total_dps: f64,
    pub rows: Vec<PlayerRow>,
    /// What is being fought, if the packet stream has revealed it (issue #9
    /// slice 2).
    pub encounter: EncounterInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crit_pct_zero_hits_is_zero() {
        let s = PlayerStats::new(1);
        assert_eq!(s.crit_pct(), 0.0);
    }

    #[test]
    fn crit_pct_computes_percentage() {
        let mut s = PlayerStats::new(1);
        s.hits = 10;
        s.crit_hits = 3;
        assert!((s.crit_pct() - 30.0).abs() < 0.001);
    }

    #[test]
    fn lucky_pct_zero_hits_is_zero() {
        let s = PlayerStats::new(1);
        assert_eq!(s.lucky_pct(), 0.0);
    }

    #[test]
    fn lucky_pct_computes_percentage() {
        let mut s = PlayerStats::new(1);
        s.hits = 4;
        s.lucky_hits = 1;
        assert!((s.lucky_pct() - 25.0).abs() < 0.001);
    }
}
