//! System test for explicit death/revive state (issue #339/#272): a decoded
//! `WorldNtf.NotifyReviveUser` (opcode `0x27`) closes a player's dead-time
//! interval at the exact revive moment instead of the inferred "next
//! action" fallback, and a recognized boss's decoded `AttrState` going dead
//! ends the fight the same way `SyncDamageInfo.is_dead` or an HP sync to 0
//! already do.
//!
//! Scripted `now_ms` values only — never wall-clock.

mod common;

use bpsr_meter::FightState;
use bpsr_test_support::scenario::Scenario;
use bpsr_test_support::wire::prof;
use common::{Rig, assert_golden};

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const M_BOSS: i64 = 2001;
const IGNISOR: u32 = 103;
const TOWERING_RUIN: u32 = 1101;

/// Aria tanks the boss the whole way through; Brin takes a killing blow,
/// is explicitly revived by `NotifyReviveUser`, and gets back in the fight
/// before the boss's own `AttrState` reports it dead.
#[test]
fn explicit_death_and_revive_and_boss_attr_state_death() {
    let scenario = Scenario::new("death_and_revive")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        // Brin takes a killing blow from the boss...
        .at(3_000)
        .player_killed_by(M_BOSS, P_BRIN, 900, 9_999)
        // ...and is explicitly revived 2_500ms later, well before Brin's
        // next action would otherwise have inferred it.
        .at(5_500)
        .revive(P_BRIN)
        .at(6_000)
        .hit(P_BRIN, M_BOSS, 202, 10_000)
        // The boss's own `AttrState` reports it dead, ending the fight —
        // no `SyncDamageInfo.is_dead` or HP-to-0 sync involved.
        .at(7_000)
        .monster_state(M_BOSS, true)
        .at(7_500)
        .tick()
        .capture("death_and_revive");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    let brin = capture
        .snapshot
        .rows
        .iter()
        .find(|r| r.uid == P_BRIN)
        .expect("Brin's row");
    assert_eq!(brin.deaths, 1);
    assert_eq!(
        brin.dead_ms,
        Some(2_500),
        "closed at the explicit revive (5_500), not Brin's next hit (6_000)"
    );

    assert_eq!(capture.fight_state, FightState::Ended);
    assert_eq!(
        capture.snapshot.duration_ms, 5_000,
        "frozen at the boss's AttrState death (7_000) minus the first hit (2_000)"
    );

    assert_golden(capture);
}
