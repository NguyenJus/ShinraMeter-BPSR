//! Per-player stats and the UI-facing snapshot read model (plan §T2.1).

use crate::event::Class;
use std::collections::HashMap;

/// Per-skill accumulator (issue #16). Keyed by the raw wire skill id — BPSR
/// self-reports a stable id on every sub-hit, so no normalisation, no
/// divide-by-100, no parent/sub-skill remap.
#[derive(Debug, Clone, Default)]
pub struct SkillStats {
    pub total_damage: i64,
    pub hits: u64,
    pub crit_hits: u64,
    pub crit_damage: i64,
    pub lucky_hits: u64,
    pub lucky_damage: i64,
    /// D7: running max of *crit* hit values. No upstream tracker keeps this
    /// (resonance-logs, bpsr-logs and BPSR-ZDPS all sum but never max a
    /// crit), so it is ours to maintain — one comparison per crit event.
    pub max_crit: i64,
}

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
    /// Each equipped slot's tier (issues #169/#170). See
    /// `event::PlayerInfo::imagine_tiers` for the full merge-rule doc —
    /// mirrors `imagines`'s `None`/`Some` semantics one-for-one.
    pub imagine_tiers: Option<[Option<i32>; 2]>,
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
    /// Per-skill breakdown (issue #16), keyed by raw skill id. Emitted in
    /// every snapshot rather than behind a subscription: resonance-logs
    /// gates its equivalent because it serialises to a webview per tick;
    /// ours is an in-process clone of at most a raid's players times a few
    /// dozen skills, and `SkillRow` carries the raw id (name resolved at
    /// draw time from the static table), so nothing allocates per skill per
    /// tick.
    pub skills: HashMap<i32, SkillStats>,
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
            imagine_tiers: None,
            total_damage: 0,
            hits: 0,
            crit_hits: 0,
            crit_damage: 0,
            lucky_hits: 0,
            lucky_damage: 0,
            deaths: 0,
            skills: HashMap::new(),
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
    /// Each equipped slot's tier (issues #169/#170), positionally paired
    /// with `imagines` — always exactly two slots, `None` per-slot meaning
    /// empty/unresolved/unknown, same convention as `imagines` itself.
    pub imagine_tiers: [Option<i32>; 2],
    pub damage: i64,
    pub dps: f64,
    pub share_pct: f32,
    pub crit_pct: f32,
    pub lucky_pct: f32,
    pub hits: u64,
    /// Times this player died this encounter (issue #49). See
    /// `PlayerStats::deaths`.
    pub deaths: u32,
    /// This player's per-skill breakdown, damage-descending (issue #16). One
    /// row per raw skill id — the reference's sub-skill "short name"
    /// grouping has no analogue in BPSR's flat ids, so there is deliberately
    /// no expander/grouping tier here (D12).
    pub skills: Vec<SkillRow>,
}

/// One row of a player's skill breakdown (issue #16). This is the contract
/// the skills-window UI consumes — do not change it without updating that
/// consumer.
#[derive(Debug, Clone)]
pub struct SkillRow {
    /// Raw wire skill id; the display name is resolved at draw time via
    /// `tables::skill_name`, so no `String` is allocated per skill per tick.
    pub skill_id: i32,
    pub damage: i64,
    /// Share of this *player's* damage, not the encounter's.
    pub share_pct: f32,
    pub crit_pct: f32,
    pub max_crit: i64,
    /// Mean crit hit; `0.0` when this skill has never crit.
    pub avg_crit: f64,
    /// D6: mean non-crit hit, `(total_damage - crit_damage) / (hits -
    /// crit_hits)`. Lucky is deliberately *not* excluded — lucky and crit
    /// are orthogonal flags in BPSR, so a lucky non-crit hit is still a
    /// white hit. `0.0` when every hit crit, matching the reference's
    /// literal `0`.
    pub avg_white: f64,
    pub avg: f64,
    pub hits: u64,
    pub crit_hits: u64,
    /// D8: hits per minute over `Meter::snapshot`'s shared
    /// `dps_duration_ms`, so a skill's rate can never disagree with the
    /// row's own DPS window.
    pub hits_per_min: f64,
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
    /// The current dungeon scene's remembered final boss (issue #125), if
    /// one has been learned — see `Meter::scene_bosses`' doc comment for how
    /// it's learned. Independent of `boss_monster_id`/`boss_name`/`is_boss`
    /// above, which remain the raw facts about the currently-selected
    /// target; `encounter_title` in `crates/app/src/ui.rs` prefers this
    /// field over them so a mid-dungeon mech (or even a genuine mid-dungeon
    /// boss) never displaces the dungeon's final boss name once it's known.
    ///
    /// `None` in a scene that can present more than one *selectable* boss
    /// (issue #150) — see `multi_boss_scene` below, and `Meter::snapshot`
    /// for the suppression itself.
    pub scene_boss_name: Option<&'static str>,
    /// Whether this scene is known to offer more than one separately
    /// selectable boss (issue #150): a raid where the party picks which of
    /// three bosses to pull. True for the curated raid scenes
    /// (`phase::is_boss_select_scene`), which is the only source — the meter
    /// cannot tell a raid's selections from an ordinary dungeon's boss order
    /// by observation. Drives `encounter_title`'s
    /// "Select a boss" placeholder in `crates/app/src/ui.rs`: with nothing
    /// engaged there is genuinely no target *yet*, as opposed to the
    /// no-target-at-all case "No target" names.
    pub multi_boss_scene: bool,
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
