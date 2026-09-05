//! System tests for the encounter-lifecycle half of the replay harness
//! (`docs/plans/system-test-harness.md` Slice C2): auto-reset triggers, the
//! post-fight freeze, and pet/summon damage attribution. The pull/join half
//! (multi-player pulls, boss-kill titles, TCP reassembly) lives in
//! `replay_pull.rs`.
//!
//! Scripted `now_ms` values only — never wall-clock.

use crate::common::{Rig, assert_golden};
use bpsr_meter::{FightEndCause, FightState, HoldKind, ResetReason};
use bpsr_protocol::{DamageEvent, DamageKind, EntityId, EntityKind, ProtocolEvent};
use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::prof;

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const P_PET_OWNER: i64 = 1004;
const PET_UID: i64 = 5001;
const M_BOSS: i64 = 2001;
const IGNISOR: u32 = 103;
const TOWERING_RUIN: u32 = 1101;
/// An open-world zone (`tables::is_dungeon_scene` is false for it), where
/// the idle timeout is still the only thing that ends a pull — issue #151
/// holds a pull open only inside an instance.
const ASTERIA_PLAINS: u32 = 7;

/// A wipe/re-pull: boss HP dips below `hp_drop_below_pct` (95%) then rolls
/// back above `hp_rollback_at_pct` (95%), which must auto-reset the
/// encounter (`crates/meter/src/reset.rs`).
///
/// Also the scenario that proves the reset is *selective*: `boss_uid`
/// (and so `boss_monster_id`/`is_boss`) is recomputed from scratch and
/// finds nothing (no enemy has `took_damage` any more), but `scene_id`
/// survives.
#[test]
fn boss_hp_rollback_auto_reset() {
    let scenario = Scenario::new("boss_hp_rollback_reset")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        // A hit is required before the HP curve: `recompute_boss` only
        // considers enemies with `took_damage == true`, so this is what
        // makes M_BOSS the selected boss at all.
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // Walk HP down below the 95% drop threshold...
        .at(2_500)
        .monster_hp(M_BOSS, 700_000)
        .at(3_000)
        .monster_hp(M_BOSS, 300_000)
        // ...then back up past the 95% rollback threshold: a fresh
        // pull/wipe, not genuine burst healing.
        .at(5_000)
        .monster_hp(M_BOSS, 1_000_000)
        .capture("boss_hp_rollback_reset");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];
    assert_eq!(capture.resets, vec![(5_000, ResetReason::BossHpRollback)]);
    assert_eq!(capture.snapshot.rows.len(), 0, "reset must clear all rows");
    assert_eq!(capture.snapshot.total_damage, 0);
    // Reset wipes `boss_uid` itself (recomputed with no enemy carrying
    // `took_damage`)...
    assert_eq!(capture.snapshot.encounter.boss_monster_id, None);
    assert!(!capture.snapshot.encounter.is_boss);
    // ...but the scene is untouched by a `BossHpRollback` reset (only
    // `ServerChanged` clears scene_id).
    assert_eq!(capture.snapshot.encounter.scene_id, Some(TOWERING_RUIN));
    // Issue #201: `scene_boss_name` now comes from the curated
    // `tables::SCENE_FINAL_BOSSES`, which does not cover this scene — nothing
    // is learned from the pull any more, so there is no caption to survive.
    assert_eq!(capture.snapshot.encounter.scene_boss_name, None);

    assert_golden(capture);
}

/// `ProtocolEvent::ServerChanged` is never decoded from bytes — its only
/// production emitter is the Windows capture thread on connection adoption
/// (`crates/capture/src/win.rs`) — so it is driven with `Scenario::inject`
/// rather than wire bytes. This is also the scenario that exercises the
/// determinism fix from Slice B: `map_event`/`Pipeline::step` take an
/// explicit `now_ms` instead of reading the wall clock for this event, so
/// running it twice must produce byte-identical goldens (checked by the
/// harness's own double-run verification, not by this test).
///
/// Issue #138: zoning/reconnecting is **not** a reset. The numbers the
/// player is still reading stay on screen; only state keyed on
/// server-session identifiers (`scene_id`, the enemy map, `boss_uid`) is
/// invalidated, and the fight clock freezes at the reconnect moment so the
/// held elapsed timer does not run while the connection is down. The next
/// fight's first hit is what clears the stats (`ResetReason::NewFight`).
///
/// Issue #152: the *header* is part of what stays on screen. The meter's
/// own live state is invalidated as above, but the snapshot describes the
/// fight being held — the boss it was against and the scene it happened
/// in — so the caption cannot drift onto the town the player walked into
/// while the rows below it are still the dungeon's.
#[test]
fn server_change_holds_the_numbers() {
    let scenario = Scenario::new("server_change_reset")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        .at(12_000)
        .inject(ProtocolEvent::ServerChanged)
        .at(13_000)
        .capture("server_change_reset");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];
    assert!(
        capture.resets.is_empty(),
        "a reconnect must not reset: {:?}",
        capture.resets
    );
    assert_eq!(
        capture.snapshot.rows.len(),
        2,
        "both damaged rows must stay on screen across the reconnect"
    );
    assert_eq!(capture.snapshot.total_damage, 65_000);
    // The clock freezes at the reconnect moment (12_000), not at the
    // capture's `now_ms` (13_000): fight_start is the first hit at 2_000.
    assert_eq!(capture.snapshot.duration_ms, 10_000);
    // ...and the header stays pinned to the fight those rows belong to
    // (issue #152), even though the meter's live scene and boss target were
    // both invalidated by the reconnect.
    assert_eq!(capture.snapshot.encounter.scene_id, Some(TOWERING_RUIN));
    assert_eq!(capture.snapshot.encounter.scene_name, Some("Towering Ruin"));
    assert_eq!(capture.snapshot.encounter.boss_monster_id, Some(IGNISOR));
    assert_eq!(capture.snapshot.encounter.boss_name, Some("Ignisor"));

    assert_golden(capture);
}

/// Damage stops, then the idle timeout (`FightConfig::idle_timeout_ms`,
/// default 9s) elapses with no further hits: the fight must freeze —
/// `duration_ms`/`total_dps`/rows held at their last-damage values, not
/// still advancing or decaying — until real combat resumes. Captured once
/// right at the freeze and again much later to prove the hold, not just the
/// initial latch (`crates/meter/src/fight.rs`).
///
/// Set in an **open-world zone** on purpose: issue #151 stops the idle
/// timeout from ending a pull while a damaged, recognized boss is alive
/// *inside an instance*, which is what `dungeon_boss_lull_does_not_end_the_pull`
/// covers. Everywhere else — a field pull, a trash pack, a boss already
/// dead — the timeout is still what freezes the meter.
///
/// Issue #313 widened that same hold to *every* scene, not just instances:
/// `IGNISOR` here is a recognized boss (`tables::is_boss_monster`) that has
/// taken damage, so the engagement-window guard keeps the fight `Active` for
/// `BOSS_ENGAGEMENT_WINDOW_MS` (60s) past the last hit before the idle
/// timeout is even allowed to bite — the wait below has to clear that window
/// first, not just the 9s idle timeout alone.
#[test]
fn idle_timeout_freeze() {
    let scenario = Scenario::new("idle_timeout_freeze")
        .at(1_000)
        .enter_scene(ASTERIA_PLAINS)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(1_500)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(3_000)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        .at(5_000)
        .hit(P_ARIA, M_BOSS, 101, 15_000)
        // 5_000 (last damage) + BOSS_ENGAGEMENT_WINDOW_MS 60_000 (the
        // engagement-window guard, issue #313) + idle_timeout_ms 9_000,
        // plus a 1s margin so the tick lands past the boundary rather than
        // exactly on it.
        .at(75_000)
        .tick()
        .capture("idle_timeout_freeze")
        // No further damage or tick: the hold must survive on its own.
        .at(121_000)
        .capture("idle_timeout_freeze_held");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 2);
    let at_freeze = &captures[0];
    let held = &captures[1];

    assert_eq!(at_freeze.fight_state, FightState::Ended);
    assert_eq!(held.fight_state, FightState::Ended);
    assert!(at_freeze.resets.is_empty());

    // The whole point: nothing about the snapshot moves between the two
    // captures despite 46s of wall-clock-equivalent time passing.
    assert_eq!(at_freeze.snapshot.duration_ms, held.snapshot.duration_ms);
    assert_eq!(at_freeze.snapshot.total_dps, held.snapshot.total_dps);
    assert_eq!(at_freeze.snapshot.rows.len(), held.snapshot.rows.len());
    for (a, b) in at_freeze
        .snapshot
        .rows
        .iter()
        .zip(held.snapshot.rows.iter())
    {
        assert_eq!(a.uid, b.uid);
        assert_eq!(a.damage, b.damage);
        assert_eq!(a.dps, b.dps);
    }
    // duration_ms = last damage (5_000) - first damage (1_500).
    assert_eq!(at_freeze.snapshot.duration_ms, 3_500);

    assert_golden(at_freeze);
    assert_golden(held);
}

/// Issue #151: a lull inside an instance is not the end of a pull. This
/// raid-shaped scenario is the one the previous test deliberately moved out
/// of the dungeon: a recognized boss, damaged and still alive, with a
/// mechanic window far longer than the 9s idle timeout in the middle of the
/// pull. Ending the fight there froze the meter mid-encounter and then
/// cleared every row the moment the party resumed (`ResetReason::NewFight`
/// is only reachable from an already-ended fight), which is exactly what
/// issue #151 was reported as.
#[test]
fn dungeon_boss_lull_does_not_end_the_pull() {
    let scenario = Scenario::new("dungeon_boss_lull")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(1_500)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(3_000)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        // The boss goes untargetable for 30s while the party runs a
        // mechanic: more than three idle timeouts, and still the same pull.
        .at(33_000)
        .tick()
        .capture("dungeon_boss_lull")
        // The party resumes on the same boss.
        .at(34_000)
        .hit(P_ARIA, M_BOSS, 101, 10_000)
        .at(34_500)
        .capture("dungeon_boss_lull_resumed");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 2);
    let mid_lull = &captures[0];
    let resumed = &captures[1];

    assert_eq!(
        mid_lull.fight_state,
        FightState::Active,
        "the pull is still live: the boss is damaged, alive, and in the instance"
    );
    assert_eq!(mid_lull.snapshot.total_damage, 65_000);
    assert!(
        resumed.resets.is_empty(),
        "resuming must not clear the pull: {:?}",
        resumed.resets
    );
    assert_eq!(
        resumed.snapshot.total_damage, 75_000,
        "the earlier damage is still on the board"
    );
    assert_eq!(resumed.snapshot.rows.len(), 2);
    // Still running, so the elapsed timer is capture time minus the first
    // hit rather than frozen at the last one.
    assert_eq!(resumed.snapshot.duration_ms, 33_000);

    assert_golden(mid_lull);
    assert_golden(resumed);
}

/// Pet/summon damage must be credited to the owner's row, not to a
/// phantom row for the pet's own uid — `decode.rs` overrides
/// `attacker_uuid` with `top_summoner_id` whenever the latter is non-zero,
/// so the pet's uid is never actually recorded as an entity attacker.
#[test]
fn pet_damage_credited_to_owner() {
    let scenario = Scenario::new("pet_damage_to_owner")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_PET_OWNER, "Nyx", prof::VERDANT_ORACLE, 10_000)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 30_000)
        .at(2_500)
        .hits(
            M_BOSS,
            vec![Hit::new(PET_UID, 303, 45_000).by_pet_of(P_PET_OWNER)],
        )
        // Deliberately well inside the idle timeout (9s from the last hit):
        // this scenario is about attribution, not the freeze (that's
        // `idle_timeout_freeze`), so the capture stays in the "still
        // active" window.
        .at(3_000)
        .capture("pet_damage_to_owner");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];
    assert_eq!(capture.snapshot.rows.len(), 2, "no phantom row for the pet");
    assert!(
        capture.snapshot.rows.iter().all(|r| r.uid != PET_UID),
        "the pet's own uid must never appear as a row"
    );
    let owner_row = capture
        .snapshot
        .rows
        .iter()
        .find(|r| r.uid == P_PET_OWNER)
        .expect("owner row must exist");
    assert_eq!(owner_row.damage, 45_000);
    assert_eq!(owner_row.hits, 1);

    assert_golden(capture);
}

/// Issue #336 step 2: a party wipe on a recognized, still-live dungeon boss
/// latches `FightEndCause::Wipe` / `HoldKind::Wipe` — the one hold none of
/// the other lifecycle goldens above pin. Both players land a hit first (so
/// each has a roster row and counts as "alive" per the wipe check), then the
/// boss kills them both; the decoder never emits a monster-attacks-player
/// damage packet in production capture (only the client's own
/// `SyncDamageInfo` entries are decoded), so this is driven with
/// `Scenario::inject` the same way `server_change_holds_the_numbers` drives
/// `ServerChanged`.
#[test]
fn party_wipe_holds_as_wipe() {
    let scenario = Scenario::new("party_wipe")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        .at(3_000)
        .inject(ProtocolEvent::Damage(DamageEvent {
            attacker: EntityId::from_display_uid(M_BOSS, EntityKind::Monster),
            attacker_uid: M_BOSS,
            attacker_kind: EntityKind::Monster,
            skill_id: 901,
            value: 12_000,
            crit: false,
            lucky: false,
            hp_lessen: 12_000,
            is_miss: false,
            is_heal: false,
            kind: DamageKind::Normal,
            target: EntityId::from_display_uid(P_ARIA, EntityKind::Player),
            target_uid: P_ARIA,
            target_kind: EntityKind::Player,
            timestamp_ms: 3_000,
            is_dead: true,
        }))
        .at(3_500)
        .inject(ProtocolEvent::Damage(DamageEvent {
            attacker: EntityId::from_display_uid(M_BOSS, EntityKind::Monster),
            attacker_uid: M_BOSS,
            attacker_kind: EntityKind::Monster,
            skill_id: 901,
            value: 11_500,
            crit: false,
            lucky: false,
            hp_lessen: 11_500,
            is_miss: false,
            is_heal: false,
            kind: DamageKind::Normal,
            target: EntityId::from_display_uid(P_BRIN, EntityKind::Player),
            target_uid: P_BRIN,
            target_kind: EntityKind::Player,
            timestamp_ms: 3_500,
            is_dead: true,
        }))
        .at(4_000)
        .capture("party_wipe");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];
    assert_eq!(capture.fight_end_cause, Some(FightEndCause::Wipe));
    assert_eq!(capture.hold_kind, Some(HoldKind::Wipe));

    assert_golden(capture);
}

/// Issue #336 step 2: a scene change mid-fight (a same-shard dungeon
/// transition, not a reconnect) latches `FightEndCause::SceneChanged` —
/// distinct from `server_change_holds_the_numbers`'s `ServerChanged`, and,
/// unlike the `party_wipe` golden above, one that never sets `HoldKind` at
/// all: `is_held`/`hold_kind` only ever name the wipe hold, so a fight cut
/// short by leaving the scene reports its cause without being "held" in
/// that sense.
#[test]
fn scene_change_mid_fight_reports_scene_changed() {
    let scenario = Scenario::new("scene_change_mid_fight")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        .at(12_000)
        .enter_scene(ASTERIA_PLAINS)
        .at(13_000)
        .capture("scene_change_mid_fight");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];
    assert_eq!(capture.fight_end_cause, Some(FightEndCause::SceneChanged));
    assert_eq!(capture.hold_kind, None);

    assert_golden(capture);
}
