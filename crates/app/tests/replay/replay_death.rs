//! System test for explicit death/revive state (issue #339/#272): a decoded
//! `WorldNtf.NotifyReviveUser` (opcode `0x27`) closes a player's dead-time
//! interval at the exact revive moment instead of the inferred "next
//! action" fallback, and a recognized boss's decoded `AttrState` going dead
//! ends the fight the same way `SyncDamageInfo.is_dead` or an HP sync to 0
//! already do.
//!
//! Scripted `now_ms` values only — never wall-clock.

use crate::common::{Rig, assert_golden};
use bpsr_meter::{FightState, HoldKind};
use bpsr_test_support::scenario::Scenario;
use bpsr_test_support::wire::prof;

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

/// Issue #366 review, finding O8: `Scenario::player_state` was never
/// exercised end to end. This is the same wipe `latch_wipe_if_party_down`
/// covers in the unit tests (issue #366 review, finding O6), driven
/// through the real decoder: Aria dies to a damage event, and Brin's death
/// — the one that leaves the whole party down — arrives only as a decoded
/// `AttrState` via `.player_state(..)`, never a `SyncDamageInfo`.
#[test]
fn attr_state_only_death_latches_a_wipe_end_to_end() {
    let scenario = Scenario::new("attr_state_wipe")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hit(P_BRIN, M_BOSS, 202, 10_000)
        // Aria falls to a damage event...
        .at(5_000)
        .player_killed_by(M_BOSS, P_ARIA, 900, 9_999)
        // ...and Brin's death, the one that leaves the party wiped, is
        // reported only via a decoded `AttrState` — no `SyncDamageInfo`
        // for it at all.
        .at(6_000)
        .player_state(P_BRIN, true)
        .at(6_500)
        .tick()
        .capture("attr_state_wipe");

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
    assert_eq!(brin.deaths, 1, "the AttrState-only death still counts");

    assert_eq!(
        capture.fight_state,
        FightState::Ended,
        "the AttrState-only death must latch the wipe just like a damage-event death"
    );
    assert_eq!(
        capture.snapshot.duration_ms, 4_000,
        "frozen at Brin's AttrState death (6_000) minus the first hit (2_000)"
    );

    assert_golden(capture);
}

/// Issue #389: an ambiguous decoded `AttrState` (issue #378's classification
/// — `Resurrection (27)` here, but anything outside the known-dead/known-alive
/// sets is the same) must not be read as an explicit alive signal and so
/// must not release an active wipe hold. `bpsr_protocol::attrs::actor_state_is_dead`
/// (exercised at the unit level in `crates/protocol/src/attrs.rs`) already
/// decodes such a state to `None` — no `EntityState` event at all — so
/// `Encounter::apply_entity_state` is never even called for it; this pins
/// that end to end, then confirms the hold *does* release on a real
/// explicit-alive signal (`ActorStateDefault`, 0). A probe against a
/// still-alive player before the wipe even happens pins the other half of
/// the misread: an ambiguous `AttrState` must never be read as a death
/// either.
#[test]
fn ambiguous_attr_state_does_not_release_a_wipe_hold() {
    let scenario = Scenario::new("ambiguous_attr_state_holds_wipe")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hit(P_BRIN, M_BOSS, 202, 25_000)
        // An ambiguous `AttrState` against a still-alive player, probed
        // before the wipe even happens, must not be misread as a death.
        .at(3_000)
        .player_state_raw(P_ARIA, 27)
        .at(3_500)
        .tick()
        .capture("ambiguous_attr_state_holds_wipe_before_wipe")
        // Both players go down, wiping the party and latching the hold.
        .at(5_000)
        .player_killed_by(M_BOSS, P_ARIA, 900, 9_999)
        .at(6_000)
        .player_state(P_BRIN, true)
        .at(6_500)
        .tick()
        .capture("ambiguous_attr_state_holds_wipe_after_wipe")
        // An ambiguous `AttrState` (Resurrection, 27) decodes to no
        // explicit alive/dead signal at all (issue #378) and so must not
        // touch the hold.
        .at(7_000)
        .player_state_raw(P_BRIN, 27)
        .at(7_500)
        .tick()
        .capture("ambiguous_attr_state_holds_wipe_after_ambiguous")
        // A real explicit-alive signal (`ActorStateDefault`, 0) does
        // release it, once few enough of the roster is still down.
        .at(8_000)
        .player_state(P_BRIN, false)
        .at(8_500)
        .tick()
        .capture("ambiguous_attr_state_holds_wipe_after_release");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);

    assert_eq!(captures.len(), 4);

    let before_wipe = &captures[0];
    assert_eq!(
        before_wipe.fight_state,
        FightState::Active,
        "an ambiguous AttrState on a live player must not be read as a death"
    );
    assert_eq!(
        before_wipe.hold_kind, None,
        "an ambiguous AttrState on a live player must not be read as a death"
    );

    let after_wipe = &captures[1];
    assert_eq!(after_wipe.fight_state, FightState::Ended);
    assert_eq!(after_wipe.hold_kind, Some(HoldKind::Wipe));

    let after_ambiguous = &captures[2];
    assert_eq!(
        after_ambiguous.fight_state,
        FightState::Ended,
        "the ambiguous AttrState must not resume/clear the held fight"
    );
    assert_eq!(
        after_ambiguous.hold_kind,
        Some(HoldKind::Wipe),
        "an ambiguous AttrState (Resurrection, 27) must not release the wipe hold"
    );

    let after_release = &captures[3];
    assert_eq!(
        after_release.fight_state,
        FightState::Ended,
        "releasing the hold lifts it but leaves the fight Ended"
    );
    assert_eq!(
        after_release.hold_kind, None,
        "an explicit alive AttrState (ActorStateDefault, 0) does release the wipe hold \
         once enough of the roster is back up"
    );

    assert_golden(before_wipe);
    assert_golden(after_wipe);
    assert_golden(after_ambiguous);
    assert_golden(after_release);
}
