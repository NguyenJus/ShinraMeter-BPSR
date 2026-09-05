//! System tests for issue #343: party membership must update on
//! `NotifyLeaveTeam` and on a full `NotifyJoinTeam` roster resync, not
//! just grow on the first join — otherwise a member who leaves or is
//! kicked lingers in the party view (and in the wipe-fraction count)
//! forever. Exercises the real decode -> meter pipeline end to end
//! (`TcpReassembler` -> `Decoder` -> `Pipeline`), unlike
//! `bpsr-protocol`/`bpsr-meter`'s own narrower unit tests for the same
//! fix.
//!
//! Scripted `now_ms` values only — never wall-clock.

use crate::common::{Rig, assert_golden};
use bpsr_test_support::scenario::Scenario;
use bpsr_test_support::wire::{self, prof};

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const P_CASS: i64 = 1003;
const M_BOSS: i64 = 2001;
const IGNISOR: u32 = 103;
const TOWERING_RUIN: u32 = 1101;

/// A member who leaves mid-pull, having never dealt damage, must not
/// linger in the party view — the bug issue #343 reports directly.
#[test]
fn a_member_who_leaves_drops_out_of_the_roster() {
    let scenario = Scenario::new("team_leave_drops_roster_member")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .team_join(vec![
            wire::team_member(P_ARIA, "Aria", prof::STORMBLADE, 12_000),
            wire::team_member(P_BRIN, "Brin", prof::FROST_MAGE, 11_000),
            wire::team_member(P_CASS, "Cass", prof::TWIN_STRIKER, 10_000),
        ])
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        .hit(P_BRIN, M_BOSS, 102, 30_000)
        .at(3_000)
        .team_leave(P_CASS)
        .at(4_000)
        .tick()
        .capture("team_leave_drops_roster_member");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    let mut uids: Vec<i64> = capture.snapshot.rows.iter().map(|r| r.uid).collect();
    uids.sort();
    assert_eq!(uids, vec![P_ARIA, P_BRIN], "Cass left and must not linger");

    assert_golden(capture);
}

/// A kicked member (`leave_type = 1`) must drop out of the roster the same
/// way a voluntary leave does (issue #343) — the decoder doesn't
/// distinguish the two, so this drives the real reassembler/decoder/
/// pipeline with `Scenario::team_kick` and asserts the same row-drop as
/// `a_member_who_leaves_drops_out_of_the_roster`.
#[test]
fn a_kicked_member_drops_out_of_the_roster() {
    let scenario = Scenario::new("team_kick_drops_roster_member")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .team_join(vec![
            wire::team_member(P_ARIA, "Aria", prof::STORMBLADE, 12_000),
            wire::team_member(P_BRIN, "Brin", prof::FROST_MAGE, 11_000),
            wire::team_member(P_CASS, "Cass", prof::TWIN_STRIKER, 10_000),
        ])
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        .hit(P_BRIN, M_BOSS, 102, 30_000)
        .at(3_000)
        .team_kick(P_CASS)
        .at(4_000)
        .tick()
        .capture("team_kick_drops_roster_member");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    let mut uids: Vec<i64> = capture.snapshot.rows.iter().map(|r| r.uid).collect();
    uids.sort();
    assert_eq!(
        uids,
        vec![P_ARIA, P_BRIN],
        "Cass was kicked and must not linger"
    );

    assert_golden(capture);
}

/// A full `NotifyJoinTeam` resync (issue #343) is not purely additive: a
/// member missing from the new roster must be pruned even though no
/// explicit `NotifyLeaveTeam` was ever seen for them (e.g. they left while
/// this meter was attached to a different scene) — and a member who stays
/// must keep the fight stats already accumulated for them, not have their
/// row rebuilt from scratch.
#[test]
fn a_full_roster_resync_prunes_a_member_missing_from_it() {
    let scenario = Scenario::new("team_roster_resync_prunes_missing_member")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .team_join(vec![
            wire::team_member(P_ARIA, "Aria", prof::STORMBLADE, 12_000),
            wire::team_member(P_BRIN, "Brin", prof::FROST_MAGE, 11_000),
        ])
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // A second `NotifyJoinTeam` push — Brin is gone from it, with no
        // `NotifyLeaveTeam` ever having been seen for them.
        .at(3_000)
        .team_join(vec![wire::team_member(
            P_ARIA,
            "Aria",
            prof::STORMBLADE,
            12_000,
        )])
        .at(4_000)
        .tick()
        .capture("team_roster_resync_prunes_missing_member");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    assert_eq!(
        capture
            .snapshot
            .rows
            .iter()
            .map(|r| r.uid)
            .collect::<Vec<_>>(),
        vec![P_ARIA],
        "Brin must be pruned by the resync"
    );
    assert_eq!(
        capture.snapshot.rows[0].damage, 50_000,
        "Aria's fight stats must survive the resync untouched"
    );

    assert_golden(capture);
}
