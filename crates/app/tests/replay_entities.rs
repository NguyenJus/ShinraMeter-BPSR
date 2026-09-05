//! Issue #335: entity identity through the real capture/protocol/pipeline
//! stack.
//!
//! The protocol boundary used to truncate every entity to `uuid >> 16`, so
//! two entities that share that number — a uid the server has recycled onto
//! something new, or a shadow/mirror copy of one that is still live — became
//! one row and their damage blended. These scenarios drive both cases end to
//! end (`common::Rig`: bytes -> `TcpReassembler` -> `Decoder` -> `Pipeline`)
//! and pin the result against a golden.

mod common;

use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::{player_uuid, prof};
use common::{Rig, assert_golden};

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const M_BOSS: i64 = 2001;

/// `tables::monster_name(103) == Some("Ignisor")`, and a recognized boss.
const IGNISOR: u32 = 103;
/// `tables::scene_name(1101) == Some("Towering Ruin")`, a dungeon scene.
const TOWERING_RUIN: u32 = 1101;

/// The client-side flag (bit 14) — one of the two flag bits `uuid >> 16`
/// throws away. An entity carrying it shares its display uid with the entity
/// that does not, which is exactly the collision issue #335 is about; the
/// summon flag (bit 15) produces the same collision for monsters.
const CLIENT_FLAG: i64 = 1 << 14;

/// Two distinct player entities wearing the same display uid, each hitting
/// the boss for a different amount, plus an ordinary third player as a
/// control.
///
/// Pre-#335 the two collapsed into a single row holding the sum. The golden
/// records the fixed behaviour: two rows, same printed uid, separate totals.
fn build_uid_recycle() -> Scenario {
    let aria = player_uuid(P_ARIA);
    let shadow_aria = aria | CLIENT_FLAG;
    assert_eq!(
        aria >> 16,
        shadow_aria >> 16,
        "the scenario is only meaningful if the two share a display uid"
    );

    Scenario::new("uid_recycle_separates_entities")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear_uuid(aria, "Aria", prof::STORMBLADE, 12_000)
        .player_appear_uuid(shadow_aria, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hits(
            M_BOSS,
            vec![
                Hit::new(P_ARIA, 101, 90_000).from_uuid(aria),
                Hit::new(P_ARIA, 101, 30_000).from_uuid(shadow_aria),
                Hit::new(P_BRIN, 202, 51_000),
            ],
        )
        .at(3_000)
        .hits(
            M_BOSS,
            vec![
                Hit::new(P_ARIA, 101, 5_000).from_uuid(aria),
                Hit::new(P_ARIA, 101, 1_000).from_uuid(shadow_aria),
            ],
        )
        // Well after the last hit, but a recognized, damaged, living boss in
        // a dungeon holds the pull open (issue #151), so the fight is still
        // active here — this scenario is about identity, not lifecycle.
        .at(30_000)
        .capture("uid_recycle_separates_entities")
}

#[test]
fn uid_recycle_separates_entities() {
    let mut rig = Rig::new();
    let captures = rig.run(&build_uid_recycle());
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    let rows = &capture.snapshot.rows;
    assert_eq!(
        rows.len(),
        3,
        "two distinct entities under display uid {P_ARIA}, plus Brin"
    );

    let mut shared: Vec<i64> = rows
        .iter()
        .filter(|r| r.uid == P_ARIA)
        .map(|r| r.damage)
        .collect();
    shared.sort_unstable();
    assert_eq!(
        shared,
        vec![31_000, 95_000],
        "each entity keeps its own total; blended they would be one 126,000 row"
    );

    assert_golden(capture);
}
