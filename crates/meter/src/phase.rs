//! Curated multi-phase boss groups (issue #124).
//!
//! A regular dungeon has one final boss, which may fight through several
//! *phases* — each phase is a distinct monster template id, and the earlier
//! phase's entity genuinely dies when the next one spawns. A raid instead has
//! up to three final bosses fought sequentially in the same instance. The
//! meter must tell those two apart: a phase change continues the fight (keep
//! the timer and every accumulated row), a new boss resets it.
//!
//! Nothing on the wire says which one just happened, and no upstream data
//! table groups phases either, so this module carries the mapping as a
//! **hand-curated table**. See [`BOSS_PHASE_GROUPS`] for the maintenance
//! story.
//!
//! Deliberately a separate module from [`crate::tables`]: that file is
//! regenerated wholesale by `scripts/gen-name-tables.py` and CI's
//! `name-tables / generated` job diffs it byte-for-byte against the
//! generator's output, so a hand-written table cannot live there.

/// Multi-phase boss fights, one inner slice per fight, listing every monster
/// template id that fight can present. Two ids in the same slice are two
/// phases (or two simultaneously-targetable parts) of **one** encounter;
/// two ids in different slices — or an id in no slice at all — are different
/// encounters.
///
/// # Why this is curated and not derived
///
/// The obvious alternative is to parse the display name: group
/// `"Paradox-Calamity Remnant - Origin"` with `"- Continuation"` and
/// `"- Final"` by their shared stem. That rule does not survive contact with
/// `tables.rs`. The same `" - "` separator carries difficulty tiers
/// (`"Towering Ruin - Hard"`), cosmetic variants (`"Flame Orc - Resonance"`,
/// `"Tina - Role"`), body parts (`"Dragonbane Golem - Right Cannon"`) and a
/// bare category prefix on 40 unrelated bosses (`"Boss - Crimson Foxen"`,
/// `"Boss - Flame Boarrier"` — a naive split makes every one of those the
/// same "fight"). The game's own phase naming also changes between seasons,
/// so a heuristic tuned to today's names would mis-group silently after a
/// content patch. A table someone edits deliberately fails loudly instead:
/// a new season's boss is simply absent until it is added, which degrades to
/// exactly today's behaviour.
///
/// # Where these ids came from
///
/// Every id below is a `tables::is_boss_monster` id (asserted by a test in
/// this module) whose `tables::monster_name` shows an explicit phase marker,
/// found by scanning `tables.rs` for boss names containing `" - "` and
/// `"Phase"`:
///
/// * **Paradox-Calamity Remnant** — `- Origin` (103100/103107/103108),
///   `- Continuation` (103200/103207/103208), `- Final`
///   (103300/103308/103309), `- Final Phantom` (103310/103311). This is the
///   fight issue #124 was filed about: the Origin phase carries the larger
///   `max_hp`, so it wins `Meter::recompute_boss` and its death used to
///   freeze the meter while the party was still fighting Continuation.
/// * **Dragonbane Golem** — `- Right Cannon` (103110/103301) and
///   `- Left Cannon` (103111/103302). One golem with two separately
///   targetable, separately killable cannons, appearing at two dungeon
///   tiers; killing one must not end the pull.
/// * **Goblin King** — `- AegisTransformation` (203) and
///   `- Staff Transformation` (204): the two forms one Goblin King
///   transforms between.
///
/// # Adding a new multi-phase boss
///
/// One edit, here: append a `&[..]` slice listing the fight's monster ids
/// (find them with `tables::monster_name`; ids for the same fight are almost
/// always adjacent in the template-id space). Requirements, both enforced by
/// the tests below so a typo fails the build rather than silently never
/// matching:
///
/// 1. every id must satisfy `tables::is_boss_monster` — a non-boss id can
///    never reach this table at runtime, because the resume path checks
///    bosshood first;
/// 2. no id may appear in two groups.
///
/// A group of one id is pointless (nothing to pair it with) — leave the
/// fight out until its sibling phase ids are known. Known-incomplete
/// candidates, listed so the next person does not have to rediscover them:
/// `"Illusion-Shroud Adjudicator - Final Form"` (920501-920505,
/// 920561-920570) and `"Thanatos - Final Form"` (33500) both name a *final*
/// form whose earlier form has no boss id in the tables; `"Seed of Malice -
/// Phase 1/2"` (69214-69216), `"Dragonbane Golem Illusion (Phase 2)"`
/// (103203) and `"Scarlet Abyss - P1/P2 Afterimage"` (103004/103005) are
/// explicitly phase-named but are **not** `is_boss_monster`, so they cannot
/// be listed until the boss table recognizes them.
#[rustfmt::skip]
const BOSS_PHASE_GROUPS: &[&[u32]] = &[
    // Paradox-Calamity Remnant: Origin -> Continuation -> Final (+ Phantom).
    &[103100, 103107, 103108, 103200, 103207, 103208, 103300, 103308, 103309, 103310, 103311],
    // Dragonbane Golem: right and left cannon, at both dungeon tiers.
    &[103110, 103111, 103301, 103302],
    // Goblin King: Aegis form and Staff form.
    &[203, 204],
];

/// Whether `a` and `b` are two *different* phases of the same curated
/// multi-phase boss fight (issue #124).
///
/// The only public entry point for the grouping, so callers never depend on
/// how [`BOSS_PHASE_GROUPS`] is stored. False when either id is ungrouped,
/// when they belong to different fights, and — deliberately — when they are
/// the *same* id: "the boss that just died is the boss being hit now" is not
/// a phase transition, it is the same entity, and treating it as one would
/// let a corpse's straggler DoT tick resurrect an ended fight.
pub fn same_phase_group(a: u32, b: u32) -> bool {
    a != b && group_of(a).is_some_and(|g| Some(g) == group_of(b))
}

/// Index of the group containing `id`, if any. Linear over a table of a
/// handful of short slices — this runs at most once per damage event while
/// a fight is being held, never in a hot loop.
fn group_of(id: u32) -> Option<usize> {
    BOSS_PHASE_GROUPS
        .iter()
        .position(|group| group.contains(&id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables;

    #[test]
    fn every_curated_phase_id_is_a_recognized_boss() {
        // A non-boss id in this table is dead weight: the resume path in
        // `Meter::apply_damage` checks `is_boss_monster` before it ever asks
        // about phase groups, so such an id could never match. Failing here
        // turns a typo (or an id the boss table dropped on refresh) into a
        // red build instead of a silently inert entry.
        for group in BOSS_PHASE_GROUPS {
            for &id in *group {
                assert!(
                    tables::is_boss_monster(id),
                    "phase-group id {id} is not a recognized boss ({:?})",
                    tables::monster_name(id)
                );
            }
        }
    }

    #[test]
    fn no_monster_id_appears_in_two_phase_groups() {
        let mut seen = std::collections::HashSet::new();
        for group in BOSS_PHASE_GROUPS {
            for &id in *group {
                assert!(seen.insert(id), "monster id {id} is in two phase groups");
            }
        }
    }

    #[test]
    fn no_phase_group_has_fewer_than_two_ids() {
        for group in BOSS_PHASE_GROUPS {
            assert!(
                group.len() >= 2,
                "a one-id phase group can never match anything: {group:?}"
            );
        }
    }

    #[test]
    fn the_three_paradox_calamity_phases_are_one_fight() {
        // Origin / Continuation / Final — the exact ids issue #124 names.
        assert!(same_phase_group(103_108, 103_207));
        assert!(same_phase_group(103_207, 103_308));
        assert!(same_phase_group(103_108, 103_308));
        // ...and symmetric.
        assert!(same_phase_group(103_308, 103_108));
    }

    #[test]
    fn the_final_phantom_is_in_the_paradox_calamity_fight() {
        assert!(same_phase_group(103_308, 103_310));
    }

    #[test]
    fn two_unrelated_recognized_bosses_are_not_the_same_fight() {
        // "Boss - Crimson Foxen" (10041) and "Boss - Flame Boarrier" (11031):
        // both recognized bosses, both named with the same `" - "` separator
        // a stem heuristic would have split on, and completely unrelated.
        assert!(tables::is_boss_monster(10_041));
        assert!(tables::is_boss_monster(11_031));
        assert!(!same_phase_group(10_041, 11_031));
    }

    #[test]
    fn a_boss_in_the_table_is_not_grouped_with_one_outside_it() {
        // Sequential raid bosses: the Paradox-Calamity Remnant's Final phase
        // against an unrelated boss must reset, not resume.
        assert!(!same_phase_group(103_308, 10_041));
        assert!(!same_phase_group(10_041, 103_308));
    }

    #[test]
    fn an_id_is_never_its_own_next_phase() {
        assert!(!same_phase_group(103_308, 103_308));
        // Also true for an id the table doesn't know at all.
        assert!(!same_phase_group(10_041, 10_041));
    }

    #[test]
    fn the_dragonbane_golem_cannons_are_one_fight() {
        assert!(same_phase_group(103_110, 103_111));
        assert!(same_phase_group(103_110, 103_302));
    }

    #[test]
    fn the_goblin_king_transformations_are_one_fight() {
        assert!(same_phase_group(203, 204));
    }

    #[test]
    fn the_golem_cannons_are_not_the_paradox_calamity_fight() {
        // Adjacent ids in the same dungeon's template block, different
        // encounters — the table must keep them apart.
        assert!(!same_phase_group(103_110, 103_108));
    }

    #[test]
    fn an_unknown_monster_id_has_no_phase_group() {
        assert!(!same_phase_group(0, 103_108));
        assert!(!same_phase_group(103_108, 0));
    }
}
