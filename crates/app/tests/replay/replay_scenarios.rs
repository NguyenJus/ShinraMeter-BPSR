//! System tests for issue #342's first batch of missing replay scenarios:
//! starting mid-instance (no `EnterScene` before damage), a server change
//! landing in a *new* instance mid-pull, a party wipe followed by a
//! re-pull, a world boss holding a pull open through a long lull, and a
//! curated multi-phase boss transition. Each scenario is driven through the
//! real capture/protocol/pipeline stack (`common::Rig`) and asserted
//! against checked-in goldens (`common::assert_golden`), the same way
//! `replay_pull.rs`/`replay_lifecycle.rs` do.
//!
//! What is deliberately *not* here: uid recycle across pulls, dungeon
//! enter/leave events, back-to-back dungeons, and app shutdown mid-fight —
//! the second batch of issue #342.

use crate::common::{Rig, assert_golden};
use bpsr_meter::{FightState, ResetReason};
use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::prof;

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const M_BOSS: i64 = 2001;
const IGNISOR: u32 = 103;
const TOWERING_RUIN: u32 = 1101;
/// A different dungeon instance (`tables::scene_name(1001) == Some("Tina's
/// Mindrealm")`, `tables::is_dungeon_scene(1001) == true`) — a genuinely
/// distinct destination from `TOWERING_RUIN`, not just a different id for
/// the same instance (1101 and 1102 both name "Towering Ruin").
const OTHER_DUNGEON: u32 = 1001;

/// Issue #342, scenario 1: the app can attach mid-instance — `ENTER_SCENE`
/// fired once, before the meter existed, so the first packets this session
/// ever sees are already mid-fight: a player, a recognized boss, and
/// damage, with no `Scene` event at all. Issue #293 taught `Meter::apply`
/// to read a *later* `Scene` packet (`SyncContainerData`'s full-state push
/// can land well after the pull is already underway) as "learn where we
/// already are" rather than as a transition that would cut the fight short
/// — covered at the `bpsr-meter` unit level by
/// `scene_learned_mid_fight_does_not_cut_it_short`, but never end to end
/// through the real decode/pipeline stack. This is that system test.
#[test]
fn starting_mid_instance() {
    let scenario = Scenario::new("starting_mid_instance")
        .at(1_000)
        // No `.enter_scene(..)`: this session's very first packets are
        // already mid-fight.
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(1_500)
        .tick()
        .capture("starting_mid_instance_no_scene")
        // The scene id finally arrives, unchanged from what it always was —
        // a learn, not a transition.
        .at(2_000)
        .enter_scene(TOWERING_RUIN)
        .at(2_500)
        .hit(P_ARIA, M_BOSS, 101, 20_000)
        .tick()
        .capture("starting_mid_instance_scene_learned");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let no_scene = &captures[0];
    let learned = &captures[1];

    assert!(
        no_scene.resets.is_empty(),
        "no Scene packet has arrived yet, so nothing has reset"
    );
    assert_eq!(no_scene.snapshot.encounter.scene_id, None);
    assert_eq!(no_scene.snapshot.total_damage, 40_000);
    assert_eq!(no_scene.fight_state, FightState::Active);

    assert!(
        learned.resets.is_empty(),
        "learning the scene mid-fight must not report a reset: {:?}",
        learned.resets
    );
    assert_eq!(learned.snapshot.encounter.scene_id, Some(TOWERING_RUIN));
    assert_eq!(
        learned.snapshot.total_damage, 60_000,
        "the pull's earlier damage must survive the scene finally being learned"
    );
    assert_eq!(learned.snapshot.rows.len(), 1);
    assert_eq!(learned.fight_state, FightState::Active);

    assert_golden(no_scene);
    assert_golden(learned);
}

/// Issue #342, scenario 2: a reconnect mid-pull (`ProtocolEvent::ServerChanged`)
/// that lands in a *different* dungeon instance. `replay_lifecycle.rs`'s
/// `server_change_holds_the_numbers` already covers the reconnect-into-the-
/// same-instance hold; this covers what that test doesn't — the
/// `ResetReason::SceneChanged` path (issue #191), which only fires
/// immediately, ahead of any new hit, when the fight was already latched
/// held going into the `Scene` event. A same-server scene change without a
/// prior `ServerChanged` defers to the ordinary `NewFight` reset instead
/// (`encounter.rs`'s `cut_short` gate) — the `ServerChanged` here is what
/// makes the immediate reset reachable at all.
#[test]
fn server_change_mid_pull_new_dungeon() {
    const M_BOSS2: i64 = 2_002;

    let scenario = Scenario::new("server_change_mid_pull_new_dungeon")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(10_000)
        .inject(bpsr_protocol::ProtocolEvent::ServerChanged)
        // A tick between the reconnect and the new instance's `Scene`
        // packet, matching the real TICK_INTERVAL (100ms) — without it the
        // cut-short fight is destroyed before `record_fight_end` observes
        // it, an ordering that can't happen live.
        .at(10_500)
        .tick()
        // The reconnect lands in a different instance, not the one just
        // left: `ResetReason::SceneChanged` fires right here, before any
        // new damage.
        .at(11_000)
        .enter_scene(OTHER_DUNGEON)
        .tick()
        .capture("server_change_mid_pull_reset")
        .at(12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS2, IGNISOR, 1_000_000, 1_000_000)
        .hit(P_BRIN, M_BOSS2, 202, 30_000)
        .tick()
        .capture("server_change_mid_pull_new_pull");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let reset = &captures[0];
    let new_pull = &captures[1];

    assert_eq!(reset.resets, vec![(11_000, ResetReason::SceneChanged)]);
    assert_eq!(
        reset.snapshot.rows.len(),
        0,
        "the old instance's roster must not survive into the new one"
    );
    assert_eq!(reset.snapshot.encounter.scene_id, Some(OTHER_DUNGEON));

    assert_eq!(
        new_pull.resets,
        vec![(11_000, ResetReason::SceneChanged)],
        "only the scene transition reset; the new pull's first hit is not a second reset"
    );
    assert_eq!(
        new_pull.snapshot.total_damage, 30_000,
        "the new instance's pull must not carry over the old instance's 40_000"
    );
    assert_eq!(new_pull.snapshot.rows.len(), 1);

    assert_golden(reset);
    assert_golden(new_pull);
}

/// Issue #342, scenario 3: a party wipe (issue #154's death-triggered hold,
/// distinct from `replay_lifecycle.rs`'s `boss_hp_rollback_auto_reset`,
/// which exercises the *other* wipe path — the HP-rollback heuristic, and
/// never shows a subsequent re-pull) followed by a re-pull on the same
/// recognized boss. A hit on a recognized boss is exactly what
/// `withholds_after_wipe` does *not* withhold, so the re-pull's first hit
/// fires an ordinary `ResetReason::NewFight` and the board starts clean —
/// unlike a curated phase transition (`boss_phase_transition` below), which
/// resumes instead of resetting.
#[test]
fn wipe_then_re_pull() {
    // A trash monster (not in `tables::is_boss_monster`) encountered during
    // the wipe hold — `withholds_after_wipe` must drop a hit on it rather
    // than treat it as a re-pull.
    const TRASH: u32 = 10_900;
    const M_TRASH: i64 = 2_003;

    let scenario = Scenario::new("wipe_then_re_pull")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // The boss's next swing kills the (only) player: `party_is_wiped`
        // requires all party members down, which a single player trivially
        // satisfies, so this latches the wipe hold.
        .at(3_000)
        .monster_hits_player(P_ARIA, Hit::new(M_BOSS, 999, 80_000).kill())
        .tick()
        .capture("wipe_hold_engaged")
        // Past `FightConfig::post_end_grace_ms` (2s), so this is not just
        // the tail of the wipe's own packet stream, but still well inside
        // the wipe hold's release window: a hit on an unrecognized trash
        // monster must be withheld, not read as a re-pull.
        .at(5_500)
        .monster_appear(M_TRASH, TRASH, 10_000, 10_000)
        .hit(P_ARIA, M_TRASH, 101, 5_000)
        .tick()
        .capture("wipe_hold_withholds_trash_hit")
        // Well inside `WIPE_HOLD_RELEASE_MS` (60s), and well past
        // `FightConfig::post_end_grace_ms` (2s) so this reads as a genuine
        // re-pull rather than the tail of the wipe's own packet stream: the
        // party re-pulls the same recognized boss.
        .at(6_000)
        .hit(P_ARIA, M_BOSS, 101, 30_000)
        .tick()
        .capture("wipe_then_re_pull");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 3);
    let wiped = &captures[0];
    let trash_hit = &captures[1];
    let repulled = &captures[2];

    assert_eq!(wiped.fight_state, FightState::Ended);
    assert!(
        wiped.resets.is_empty(),
        "the wipe latches the fight end but is not itself a ResetReason: {:?}",
        wiped.resets
    );
    assert_eq!(
        wiped.snapshot.total_damage, 50_000,
        "the wiped attempt's damage stays on screen"
    );
    assert_eq!(wiped.snapshot.rows.len(), 1);

    assert_eq!(
        trash_hit.fight_state,
        FightState::Ended,
        "a hit on an unrecognized trash monster does not resume or reset the held wipe"
    );
    assert!(
        trash_hit.resets.is_empty(),
        "the trash hit is withheld, not treated as a new fight: {:?}",
        trash_hit.resets
    );
    assert_eq!(
        trash_hit.snapshot.total_damage, 50_000,
        "the withheld trash hit must not be added onto the wiped attempt's numbers"
    );
    assert_eq!(trash_hit.snapshot.rows.len(), 1);

    assert_eq!(repulled.fight_state, FightState::Active);
    assert_eq!(repulled.resets, vec![(6_000, ResetReason::NewFight)]);
    assert_eq!(
        repulled.snapshot.total_damage, 30_000,
        "the re-pull starts clean, not accumulated onto the wiped attempt's 50_000"
    );
    assert_eq!(repulled.snapshot.rows.len(), 1);

    assert_golden(wiped);
    assert_golden(trash_hit);
    assert_golden(repulled);
}

/// Issue #342, scenario 4: a World Dominator arena boss (issue #313's own
/// example — scene 7152, monster 3000063 "Denvel") holding a pull open
/// through a lull far longer than the 9s idle timeout, driven through the
/// real wire/pipeline stack (the `bpsr-meter` unit test
/// `the_world_dominator_arena_boss_holds_the_pull_open` covers the same
/// case at the `Meter::apply` level only).
#[test]
fn world_boss_held_pull() {
    const WORLD_BOSS_SCENE: u32 = 7_152;
    const DENVEL: u32 = 3_000_063;
    const M_DENVEL: i64 = 2_500;

    let scenario = Scenario::new("world_boss_held_pull")
        .at(1_000)
        .enter_scene(WORLD_BOSS_SCENE)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_DENVEL, DENVEL, 50_000_000, 50_000_000)
        .hit(P_ARIA, M_DENVEL, 101, 60_000)
        // 1_000 (last hit) + idle_timeout_ms (9_000) + margin: well past the
        // idle timeout, still well inside BOSS_ENGAGEMENT_WINDOW_MS (60s).
        .at(10_500)
        .tick()
        .capture("world_boss_lull_held")
        // The lull ends; the same boss resumes taking damage.
        .at(10_600)
        .hit(P_ARIA, M_DENVEL, 101, 25_000)
        .tick()
        .capture("world_boss_resumed");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let lull = &captures[0];
    let resumed = &captures[1];

    assert_eq!(
        lull.fight_state,
        FightState::Active,
        "a recognized, damaged world boss holds the pull open past the idle timeout"
    );
    assert!(lull.resets.is_empty());
    assert_eq!(lull.snapshot.total_damage, 60_000);
    assert!(lull.snapshot.encounter.is_boss);
    assert_eq!(lull.snapshot.encounter.boss_name, Some("Denvel"));

    assert_eq!(resumed.fight_state, FightState::Active);
    assert!(
        resumed.resets.is_empty(),
        "resuming the same boss must not reset the pull: {:?}",
        resumed.resets
    );
    assert_eq!(
        resumed.snapshot.total_damage, 85_000,
        "the lull's damage and the resumed damage are the same pull"
    );

    assert_golden(lull);
    assert_golden(resumed);
}

/// Issue #342, scenario 5: a curated multi-phase boss (issue #124) — the
/// Dragonbane Golem's Right Cannon (103110) dying and the Left Cannon
/// (103111) picking the fight back up within `phase_resume_window_ms` (60s)
/// — resumes the held fight instead of resetting it: the timer and every
/// accumulated row survive the phase change, unlike a genuinely different
/// boss pull (`wipe_then_re_pull` above), which starts clean.
#[test]
fn boss_phase_transition() {
    const RIGHT_CANNON: u32 = 103_110;
    const LEFT_CANNON: u32 = 103_111;
    const M_PHASE1: i64 = 2_600;
    const M_PHASE2: i64 = 2_601;

    let scenario = Scenario::new("boss_phase_transition")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_PHASE1, RIGHT_CANNON, 500_000, 500_000)
        .at(2_000)
        .hit(P_ARIA, M_PHASE1, 101, 40_000)
        .at(3_000)
        .hits(M_PHASE1, vec![Hit::new(P_ARIA, 101, 20_000).kill()])
        .tick()
        .capture("phase1_kill")
        // The Left Cannon phase appears and is engaged well within the 60s
        // resume window.
        .at(3_500)
        .monster_appear(M_PHASE2, LEFT_CANNON, 500_000, 500_000)
        .hit(P_ARIA, M_PHASE2, 101, 30_000)
        .tick()
        .capture("phase2_resumed");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let phase1 = &captures[0];
    let phase2 = &captures[1];

    assert_eq!(phase1.fight_state, FightState::Ended);
    assert!(phase1.resets.is_empty());
    assert_eq!(phase1.snapshot.total_damage, 60_000);
    assert_eq!(
        phase1.snapshot.duration_ms, 1_000,
        "fight_start (2_000) to the kill (3_000)"
    );

    assert_eq!(
        phase2.fight_state,
        FightState::Active,
        "the next phase resumes the fight rather than starting a fresh one"
    );
    assert!(
        phase2.resets.is_empty(),
        "a same-phase-group resume must not report a reset: {:?}",
        phase2.resets
    );
    assert_eq!(
        phase2.snapshot.total_damage, 90_000,
        "phase 2's damage is added onto phase 1's, not reset to just 30_000"
    );
    assert_eq!(
        phase2.snapshot.duration_ms, 1_500,
        "fight_start (2_000) is preserved across the phase change, to the capture at 3_500"
    );

    assert_golden(phase1);
    assert_golden(phase2);
}
