//! System tests for the encounter-lifecycle half of the replay harness
//! (`docs/plans/system-test-harness.md` Slice C2): auto-reset triggers, the
//! post-fight freeze, and pet/summon damage attribution. The pull/join half
//! (multi-player pulls, boss-kill titles, TCP reassembly) lives in
//! `replay_pull.rs`.
//!
//! Scripted `now_ms` values only — never wall-clock.

mod common;

use bpsr_meter::{FightState, ResetReason};
use bpsr_protocol::ProtocolEvent;
use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::prof;
use common::{Rig, assert_golden};

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const P_PET_OWNER: i64 = 1004;
const PET_UID: i64 = 5001;
const M_BOSS: i64 = 2001;
const RATHALOS: u32 = 103;
const TOWERING_RUIN: u32 = 1101;

/// A wipe/re-pull: boss HP dips below `hp_drop_below_pct` (60%) then rolls
/// back up to `hp_rollback_at_pct` (90%), which must auto-reset the
/// encounter (`crates/meter/src/reset.rs`).
///
/// Also the scenario that proves the reset is *selective*: `boss_uid`
/// (and so `boss_monster_id`/`is_boss`) is recomputed from scratch and
/// finds nothing (no enemy has `took_damage` any more), but `scene_id` and
/// the latched `scene_boss_name` (issue #125 — a dungeon's final boss is
/// session-lifetime, deliberately not cleared by `Meter::reset`) survive.
#[test]
fn boss_hp_rollback_auto_reset() {
    let scenario = Scenario::new("boss_hp_rollback_reset")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, RATHALOS, 1_000_000, 1_000_000)
        // A hit is required before the HP curve: `recompute_boss` only
        // considers enemies with `took_damage == true`, so this is what
        // makes M_BOSS the selected boss at all.
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // Walk HP down below the 60% drop threshold...
        .at(2_500)
        .monster_hp(M_BOSS, 700_000)
        .at(3_000)
        .monster_hp(M_BOSS, 300_000)
        // ...then back up past the 90% rollback threshold: a fresh
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
    // ...but the scene and its latched final-boss name are untouched by a
    // `BossHpRollback` reset (only `ServerChanged` clears scene_id).
    assert_eq!(capture.snapshot.encounter.scene_id, Some(TOWERING_RUIN));
    assert_eq!(capture.snapshot.encounter.scene_boss_name, Some("Rathalos"));

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
#[test]
fn server_change_reset() {
    let scenario = Scenario::new("server_change_reset")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, RATHALOS, 1_000_000, 1_000_000)
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
    assert_eq!(capture.resets, vec![(12_000, ResetReason::ServerChange)]);
    assert_eq!(capture.snapshot.rows.len(), 0, "reset must clear all rows");
    // ServerChanged clears scene_id/enemies/boss_uid, unlike a boss-HP-
    // rollback reset.
    assert_eq!(capture.snapshot.encounter.scene_id, None);
    assert_eq!(capture.snapshot.encounter.boss_monster_id, None);
    assert_eq!(capture.snapshot.encounter.scene_boss_name, None);

    assert_golden(capture);
}

/// Damage stops, then the idle timeout (`FightConfig::idle_timeout_ms`,
/// default 9s) elapses with no further hits: the fight must freeze —
/// `duration_ms`/`total_dps`/rows held at their last-damage values, not
/// still advancing or decaying — until real combat resumes. Captured once
/// right at the freeze and again 46s later to prove the hold, not just the
/// initial latch (`crates/meter/src/fight.rs`).
#[test]
fn idle_timeout_freeze() {
    let scenario = Scenario::new("idle_timeout_freeze")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, RATHALOS, 5_000_000, 5_000_000)
        .at(1_500)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(3_000)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        .at(5_000)
        .hit(P_ARIA, M_BOSS, 101, 15_000)
        // 5_000 (last damage) + idle_timeout_ms 9_000.
        .at(14_000)
        .tick()
        .capture("idle_timeout_freeze")
        // No further damage or tick: the hold must survive on its own.
        .at(60_000)
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
        .monster_appear(M_BOSS, RATHALOS, 5_000_000, 5_000_000)
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
