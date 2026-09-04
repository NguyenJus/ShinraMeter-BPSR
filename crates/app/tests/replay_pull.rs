//! Slice C1 (`docs/plans/system-test-harness.md` §5, "Slice C — scenarios
//! and goldens" / C1): synthetic pull scenarios driven through the real
//! capture/protocol/pipeline stack (`common::Rig`) and asserted against
//! checked-in goldens (`common::assert_golden`).
//!
//! Covers: a multi-player pull with crit/lucky/miss hits and the
//! `SyncContainerData` identity path (`multi_player_pull`), a boss kill that
//! freezes the fight and latches the encounter title (`boss_kill_title` /
//! `boss_kill_title_held`), and TCP-segmented delivery of the exact same
//! byte stream as `multi_player_pull` — including a mid-frame split and an
//! out-of-order segment — asserted against the *same* golden file to prove
//! reassembly is transparent to the meter (`tcp_segmented_pull`).

mod common;

use bpsr_meter::FightState;
use bpsr_test_support::scenario::{Delivery, Hit, Scenario, Step};
use bpsr_test_support::wire::prof;
use common::{Rig, assert_golden};

// Shared uids for this file's scenarios. `common/mod.rs` (Slice B) doesn't
// define these — the plan's fallback rule (§5 Slice C) is to put a missing
// shared constant in the scenario file that needs it.
const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const P_CADE: i64 = 1003;
const M_BOSS: i64 = 2001;

/// Table ids picked in the plan (§0.7) because they satisfy both the name
/// lookup and the boss/dungeon membership test.
mod monster {
    /// `tables::monster_name(103) == Some("Ignisor")`,
    /// `tables::is_boss_monster(103) == true`.
    pub const IGNISOR: u32 = 103;
}
mod scene {
    /// `tables::scene_name(1101) == Some("Towering Ruin")`,
    /// `tables::is_dungeon_scene(1101) == true`.
    pub const TOWERING_RUIN: u32 = 1101;
}

/// Per-verb delivery override for the two multi-hit steps of
/// [`build_multi_player_pull`], so `tcp_segmented_pull` can replay the exact
/// same scenario with TCP-layer segmentation while every other test gets the
/// default `Delivery::Whole`.
struct Deliveries {
    crit_hits: Delivery,
    lucky_hits: Delivery,
}

impl Default for Deliveries {
    fn default() -> Self {
        Self {
            crit_hits: Delivery::Whole,
            lucky_hits: Delivery::Whole,
        }
    }
}

/// Three players with distinct names, professions, fight points and
/// (crucially, per §0.4.3's tie guard) distinct damage totals:
/// Aria (95_000) > Brin (51_000) > Cade (27_000). Exercises a crit, a lucky
/// hit, a miss, the `SyncContainerData` identity path, and mixed
/// `compressed(true)/(false)` framing.
fn build_multi_player_pull(
    name: &'static str,
    capture_label: &'static str,
    d: Deliveries,
) -> Scenario {
    Scenario::new(name)
        .at(1_000)
        .enter_scene(scene::TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .player_appear(P_CADE, "Cade", prof::TWIN_STRIKER, 10_000)
        // Exercises the `SyncContainerData` path (keys on `char_id` as the
        // uid directly) with the same identity Aria already appeared with.
        .container_data(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, monster::IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .compressed(true)
        .next_delivery(d.crit_hits)
        .hits(
            M_BOSS,
            vec![
                // Split into several entries (still summing to the same
                // per-player totals) so this step's frame carries enough
                // `SyncDamageInfo` entries to comfortably exceed 100 bytes —
                // `tcp_segmented_pull` relies on that to prove a genuine
                // mid-frame split.
                Hit::new(P_ARIA, 101, 20_000).crit(),
                Hit::new(P_ARIA, 101, 20_000).crit(),
                Hit::new(P_ARIA, 101, 15_000).crit(),
                Hit::new(P_BRIN, 202, 16_000),
                Hit::new(P_BRIN, 202, 15_000),
            ],
        )
        .at(3_000)
        .compressed(false)
        .next_delivery(d.lucky_hits)
        .hits(
            M_BOSS,
            vec![
                Hit::new(P_BRIN, 202, 20_000).lucky(),
                Hit::new(P_CADE, 303, 15_000),
            ],
        )
        .at(3_500)
        .hits(
            M_BOSS,
            vec![
                Hit::new(P_CADE, 303, 12_000),
                Hit {
                    miss: true,
                    ..Hit::new(P_CADE, 303, 8_000)
                },
            ],
        )
        // Captured well after the last hit, but the boss is a recognized,
        // damaged, still-living boss in a dungeon scene, so the pull is
        // still live (issue #151) and `duration_ms` runs to the capture.
        // This scenario is about identity and damage attribution; the
        // fight-lifecycle cases live in `replay_lifecycle.rs`.
        .at(30_000)
        .capture(capture_label)
}

/// C1.1: names, classes, `ability_score`, share %, crit %, lucky %, hits and
/// damage-order all come out of the real decode/pipeline stack correctly.
#[test]
fn multi_player_pull() {
    let scenario = build_multi_player_pull(
        "multi_player_pull",
        "multi_player_pull",
        Deliveries::default(),
    );

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    // Hand-computed sanity checks (see the module doc / final report for the
    // full arithmetic) independent of the golden file, so a corrupted golden
    // can't silently rubber-stamp a regression.
    assert_eq!(capture.snapshot.rows.len(), 3);
    assert_eq!(capture.snapshot.total_damage, 173_000);
    let aria = capture
        .snapshot
        .rows
        .iter()
        .find(|r| r.uid == P_ARIA)
        .unwrap();
    let brin = capture
        .snapshot
        .rows
        .iter()
        .find(|r| r.uid == P_BRIN)
        .unwrap();
    let cade = capture
        .snapshot
        .rows
        .iter()
        .find(|r| r.uid == P_CADE)
        .unwrap();
    assert_eq!(aria.damage, 95_000);
    assert_eq!(brin.damage, 51_000);
    assert_eq!(cade.damage, 27_000);
    assert!(aria.damage > brin.damage && brin.damage > cade.damage);
    assert_eq!(aria.hits, 4); // t=2_000 hit + three t=2_500 crits
    assert_eq!(brin.hits, 3); // two t=2_500 hits + one t=3_000 lucky hit
    assert_eq!(cade.hits, 3); // includes the miss, which still counts as a hit
    assert!((aria.crit_pct - 75.0).abs() < 0.01); // 3 of 4 hits crit
    assert!((brin.lucky_pct - 100.0 / 3.0).abs() < 0.01); // 1 of 3 hits lucky

    assert_golden(capture);
}

/// Issue #338: a shield-absorbed hit and an immune hit must not be
/// misreported as dealt damage — they land in their own channels instead,
/// alongside an ordinary hit from the same player so the golden also pins
/// that `Normal`-kind damage is unaffected.
#[test]
fn absorbed_and_immune_hits() {
    let scenario = Scenario::new("absorbed_and_immune_hits")
        .at(1_000)
        .enter_scene(scene::TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, monster::IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 10_000)
        .at(2_500)
        .hits(
            M_BOSS,
            vec![
                Hit::new(P_ARIA, 101, 8_000).absorbed(),
                Hit::new(P_ARIA, 101, 0).immune(),
            ],
        )
        .at(30_000)
        .capture("absorbed_and_immune_hits");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    // Hand-computed sanity checks, independent of the golden file.
    assert_eq!(capture.snapshot.rows.len(), 1);
    let aria = &capture.snapshot.rows[0];
    assert_eq!(
        aria.damage, 10_000,
        "only the Normal-kind hit counts as dealt damage"
    );
    assert_eq!(aria.absorbed_total, 8_000);
    assert_eq!(aria.immune_total, 0);
    assert_eq!(
        aria.hits, 3,
        "every hit counts as a swing, absorbed/immune included"
    );
    assert_eq!(capture.snapshot.total_damage, 10_000);
    assert_eq!(capture.snapshot.total_absorbed, 8_000);
    assert_eq!(capture.snapshot.total_immune, 0);

    assert_golden(capture);
}

/// C1.2: a boss kill freezes the fight (`end_on_boss_death`), and both an
/// immediate capture and one 45s later must show the identical held
/// snapshot — the `EncounterInfo` title fields and the boss-death freeze.
#[test]
fn boss_kill_title() {
    let scenario = Scenario::new("boss_kill_title")
        .at(1_000)
        .enter_scene(scene::TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, monster::IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        .at(2_500)
        .hits(M_BOSS, vec![Hit::new(P_BRIN, 202, 25_000)])
        .at(3_000)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 35_000).kill()])
        .tick()
        .capture("boss_kill_title")
        .at(48_000) // kill timestamp (3_000) + 45_000
        .tick()
        .capture("boss_kill_title_held");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let at_kill = &captures[0];
    let held = &captures[1];

    // The fight froze at the kill (Aria 40_000 + 35_000 = 75_000, Brin
    // 25_000; total 100_000 over a 1_000ms dps window starting at the first
    // hit, ms 2_000, ending at the kill, ms 3_000).
    for capture in [at_kill, held] {
        assert_eq!(capture.fight_state, FightState::Ended);
        assert_eq!(capture.snapshot.duration_ms, 1_000);
        assert_eq!(capture.snapshot.total_damage, 100_000);
        assert!((capture.snapshot.total_dps - 100_000.0).abs() < 0.01);
        assert_eq!(capture.snapshot.encounter.boss_name, Some("Ignisor"));
        assert!(capture.snapshot.encounter.is_boss);
        assert_eq!(capture.snapshot.encounter.scene_name, Some("Towering Ruin"));
        // Issue #201: the header names the boss through the *live* lock
        // (`boss_name`/`is_boss` above), not through the curated
        // `tables::SCENE_FINAL_BOSSES` — which does not cover this scene.
        assert_eq!(capture.snapshot.encounter.scene_boss_name, None);
    }
    assert_eq!(at_kill.snapshot.duration_ms, held.snapshot.duration_ms);
    assert_eq!(at_kill.snapshot.total_dps, held.snapshot.total_dps);
    assert_eq!(at_kill.snapshot.rows.len(), held.snapshot.rows.len());

    assert_golden(at_kill);
    assert_golden(held);
}

/// C1.3: byte-identical to `multi_player_pull`, but the two multi-hit steps
/// are delivered as split/reordered TCP segments — one split lands mid-frame
/// (proven below: the split step's frame exceeds 100 bytes and the offsets
/// are strictly interior), and the lucky-hits segment is delivered
/// out-of-order. Asserted against `multi_player_pull`'s own golden file:
/// reassembly must be completely transparent to the meter.
#[test]
fn tcp_segmented_pull() {
    // Probe run (default `Delivery::Whole`) just to learn the exact frame
    // lengths — bytes are identical to the real run below since `Delivery`
    // only changes how the Rig feeds the TcpReassembler, never the bytes.
    let probe = build_multi_player_pull("probe", "probe", Deliveries::default());
    let crit_hits_len = step_bytes_len(&probe, 7);
    let lucky_hits_len = step_bytes_len(&probe, 8);
    assert!(
        crit_hits_len > 100,
        "crit-hits frame must exceed 100 bytes to prove a genuine mid-frame \
         split, got {crit_hits_len}"
    );
    // Interior offsets (0 < offset < len): the split necessarily falls
    // inside this single step's one frame, never at a frame boundary.
    let split_at = vec![crit_hits_len / 3, 2 * crit_hits_len / 3];
    assert!(split_at[0] > 0 && split_at[1] < crit_hits_len);

    let deliveries = Deliveries {
        crit_hits: Delivery::SplitAt(split_at),
        lucky_hits: Delivery::SplitAndReorder {
            at: vec![lucky_hits_len / 2],
            // Segment 1 (the back half of the frame) is pushed before
            // segment 0 (the front half) — arrives before the piece that
            // precedes it in the stream.
            order: vec![1, 0],
        },
    };
    let scenario = build_multi_player_pull("tcp_segmented_pull", "multi_player_pull", deliveries);

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0];

    // Same arithmetic as `multi_player_pull` — reassembly must be
    // transparent.
    assert_eq!(capture.snapshot.total_damage, 173_000);
    assert_eq!(capture.snapshot.rows.len(), 3);

    assert_golden(capture);
}

fn step_bytes_len(scenario: &Scenario, idx: usize) -> usize {
    match &scenario.steps[idx] {
        Step::Bytes { bytes, .. } => bytes.len(),
        other => panic!("expected Step::Bytes at index {idx}, got {other:?}"),
    }
}
