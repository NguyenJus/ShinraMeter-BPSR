//! System tests for issue #342's second batch of missing replay scenarios:
//! a monster uid recycled onto a different monster in the next pull, a
//! dungeon's own enter/leave flow signals (`SyncDungeonData`) alongside
//! ordinary scene changes, two dungeons pulled back to back, and an app
//! shutdown (the whole pipeline dropped) mid-pull. Each scenario is driven
//! through the real capture/protocol/pipeline stack (`common::Rig`) and
//! asserted against checked-in goldens (`common::assert_golden`), the same
//! way `replay_scenarios.rs` (issue #342's first batch) does.
//!
//! Scenario 6 (uid recycle) needed no DSL changes — `Scenario::monster_appear`
//! already lets a test resend the same uid with a different `monster_id`.
//! Scenarios 7 and 8 needed one new verb, `Scenario::dungeon_state`, wrapping
//! a new wire builder, `wire::dungeon_sync_data_payload` (`SyncDungeonData`,
//! `opcode::SYNC_DUNGEON_DATA`, `0x17`) — the plain-protobuf half of issue
//! #139's dungeon-flow channel; the other half, the blob-wrapped
//! `SyncDungeonDirtyData` (`0x18`), is exercised at the `bpsr-protocol` unit
//! level already (`decode.rs`'s `DUNGEON_*_HEX` fixtures) and is not needed
//! here — `on_sync_dungeon_data`/`on_sync_dungeon_dirty_data` both funnel
//! into the same `ProtocolEvent::DungeonState`, so `0x17` alone is enough to
//! exercise the meter's reaction to it end to end.
//!
//! ## `was_tracked_boss`/`recompute_boss` ordering
//!
//! A recognized boss's first-ever hit only *becomes* `boss_uid` once
//! `recompute_boss` runs on it — a hit that is simultaneously both the
//! target's first-ever damage and its killing blow reads `was_tracked_boss`
//! (computed just ahead of that recompute) as `false` and never ends the
//! fight. A real pull always lands an ordinary hit before the kill; several
//! scenarios below establish that ordering before landing a kill.

mod common;

use bpsr_app::history::sqlite::SqliteHistory;
use bpsr_app::history::{HistoryStore, RetentionPolicy};
use bpsr_meter::{FightState, ResetReason};
use bpsr_protocol::ProtocolEvent;
use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::prof;
use common::{Rig, assert_golden};
use std::path::PathBuf;

/// Deletes the backing sqlite file on drop, so a temp db is cleaned up even
/// if the test panics partway through.
struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const P_ARIA: i64 = 1001;
const M_BOSS: i64 = 2001;
const M_BOSS_2: i64 = 2002;
const IGNISOR: u32 = 103;
/// A World Dominator arena boss (issue #313), unrelated to `IGNISOR`'s
/// family — see `replay_scenarios.rs`'s `world_boss_held_pull` for the same
/// id.
const DENVEL: u32 = 3_000_063;
const TOWERING_RUIN: u32 = 1101;
/// A different dungeon instance from `TOWERING_RUIN` — see
/// `replay_scenarios.rs`'s doc comment on this same constant.
const OTHER_DUNGEON: u32 = 1001;
/// An open-world zone (`tables::is_dungeon_scene` is false for it) —
/// mirrors `replay_history.rs`'s `ASTERIA_PLAINS`.
const ASTERIA_PLAINS: u32 = 7;

/// Raw `EDungeonState` wire values (`bpsr_protocol::event::EDungeonState`'s
/// `From<i32>`, issue #139). Named here rather than imported: `test-support`
/// deliberately stays free of a typed dependency on this enum (see
/// `wire::dungeon_sync_data_payload`'s doc comment) and this scenario file
/// only ever needs the two states below.
const DUNGEON_PLAYING: i32 = 3;
const DUNGEON_END: i32 = 4;

/// Issue #342, scenario 6: a monster uid recycled across pulls. The same
/// slot (`M_BOSS = 2001`) that named a recognized boss in the pull that just
/// ended (`IGNISOR`) is reassigned by the server to an entirely unrelated
/// recognized boss (`DENVEL`, a different id family) for the next pull —
/// exactly the shape issue #317's `bpsr-meter` unit test
/// (`encounter.rs`'s `monster_id_reset` module) exercises at the
/// `Meter::apply`/`EnemyState` level. This is that same behavior end to end:
/// the recycled uid's new identity, and the new pull's damage, must not
/// blend with what the old occupant of that uid ever did.
#[test]
fn uid_recycle_across_pulls() {
    let scenario = Scenario::new("uid_recycle_across_pulls")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        // See the module doc on was_tracked_boss/recompute_boss ordering.
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        .at(2_100)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 40_000).kill()])
        .tick()
        .capture("uid_recycle_pull1_ignisor_defeated")
        // Well within `phase_resume_window_ms` (60_000ms) of the first
        // pull's end, the same uid (2001) is handed to a completely
        // different recognized boss for the next pull — the server
        // recycling a despawned monster's slot, not the same entity
        // resyncing. Landing inside the resume window is deliberate: it is
        // what actually exercises uid-recycle handling instead of an
        // ordinary NewFight reset that would happen to look the same from
        // outside the window.
        .at(30_000)
        .monster_appear(M_BOSS, DENVEL, 50_000_000, 50_000_000)
        .hit(P_ARIA, M_BOSS, 101, 35_000)
        .tick()
        .capture("uid_recycle_pull2_denvel_fresh");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let pull1 = &captures[0];
    let pull2 = &captures[1];

    assert_eq!(pull1.fight_state, FightState::Ended);
    assert!(pull1.resets.is_empty());
    assert_eq!(pull1.snapshot.total_damage, 90_000);
    assert_eq!(pull1.snapshot.encounter.boss_monster_id, Some(IGNISOR));
    assert_eq!(pull1.snapshot.encounter.boss_name, Some("Ignisor"));

    assert_eq!(pull2.fight_state, FightState::Active);
    assert_eq!(pull2.resets, vec![(30_000, ResetReason::NewFight)]);
    assert_eq!(
        pull2.snapshot.total_damage, 35_000,
        "the recycled uid's new pull must not blend onto the old occupant's 90_000"
    );
    assert_eq!(pull2.snapshot.rows.len(), 1);
    assert_eq!(
        pull2.snapshot.encounter.boss_monster_id,
        Some(DENVEL),
        "the recycled uid's identity is the new occupant, not the old one"
    );
    assert_eq!(pull2.snapshot.encounter.boss_name, Some("Denvel"));

    assert_golden(pull1);
    assert_golden(pull2);
}

/// Issue #342, scenario 7: a dungeon's own flow signals (issue #139,
/// `SyncDungeonData`/`opcode::SYNC_DUNGEON_DATA`) alongside ordinary scene
/// changes. Three things this proves, in order:
///
/// 1. **Entering starts clean.** `EDungeonState::Playing` forces a fresh
///    encounter (`ResetReason::DungeonStarted`) even over a fight already
///    in progress — the same "attach mid-fight, then learn where you are"
///    shape `replay_scenarios.rs`'s `starting_mid_instance` covers for a
///    late `Scene`, but for the dungeon's own start signal arriving late.
/// 2. **The dungeon's own end signal ends the fight appropriately.**
///    `EDungeonState::End` latches the fight end (`FightEndCause::
///    DungeonEnded`) without itself being a `ResetReason` — more
///    authoritative than the idle-timeout heuristic, and immediate.
/// 3. **Leaving to the open world holds, it does not clear.** A plain
///    `Scene` transition to a *non*-dungeon destination after the fight has
///    already ended leaves the held numbers exactly as they were (issue
///    #152) — only entering a *different dungeon* scene resets
///    (`encounter.rs`'s `entering_dungeon` gate); leaving one for open
///    world never does.
#[test]
fn dungeon_enter_leave_events() {
    let scenario = Scenario::new("dungeon_enter_leave_events")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        // Damage lands before the instance's own "you're playing" signal
        // catches up to it — same late-signal shape `starting_mid_instance`
        // covers for `Scene`, here for `DungeonState`.
        .at(1_500)
        .hit(P_ARIA, M_BOSS, 101, 40_000)
        // Observed before the dungeon's own "you're playing" signal
        // arrives: the pull is already running, entirely on the ordinary
        // `Scene`/damage path, with no reset yet at all.
        .at(1_600)
        .tick()
        .capture("dungeon_pre_entry_pull_in_progress")
        .at(2_000)
        .dungeon_state(TOWERING_RUIN, DUNGEON_PLAYING)
        .tick()
        .capture("dungeon_entered_starts_clean")
        // The real pull begins now that the instance is confirmed started.
        .at(2_500)
        .hit(P_ARIA, M_BOSS, 101, 60_000)
        .at(3_000)
        .dungeon_state(TOWERING_RUIN, DUNGEON_END)
        .tick()
        .capture("dungeon_ended_by_flow_signal")
        // The party zones out to an ordinary open-world scene.
        .at(4_000)
        .enter_scene(ASTERIA_PLAINS)
        .tick()
        .capture("leaving_to_open_world_holds_the_numbers");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 4);
    let pre_entry = &captures[0];
    let entered = &captures[1];
    let ended = &captures[2];
    let left = &captures[3];

    assert_eq!(
        pre_entry.fight_state,
        FightState::Active,
        "the pull is already running before the dungeon's own Playing signal arrives"
    );
    assert!(pre_entry.resets.is_empty());
    assert_eq!(pre_entry.snapshot.total_damage, 40_000);

    assert_eq!(entered.resets, vec![(2_000, ResetReason::DungeonStarted)]);
    assert_eq!(
        entered.fight_state,
        FightState::Idle,
        "entering the dungeon clears the in-progress pull to a clean slate"
    );
    assert_eq!(entered.snapshot.total_damage, 0);
    assert_eq!(entered.snapshot.rows.len(), 0);

    assert_eq!(
        ended.fight_state,
        FightState::Ended,
        "the dungeon's own End signal ends the fight immediately"
    );
    assert_eq!(
        ended.resets, entered.resets,
        "an End signal is a fight end, not itself a ResetReason"
    );
    assert_eq!(
        ended.snapshot.total_damage, 60_000,
        "only the damage since the clean restart, not the pre-entry 40_000"
    );
    assert_eq!(ended.snapshot.rows.len(), 1);

    assert_eq!(
        left.fight_state,
        FightState::Ended,
        "issue #152: the numbers stay on screen out in the open world"
    );
    assert_eq!(
        left.resets, ended.resets,
        "leaving to a non-dungeon destination fires no reset at all"
    );
    assert_eq!(left.snapshot.total_damage, 60_000);
    assert_eq!(left.snapshot.rows.len(), 1);
    // Issue #152's `fight_identity` pin: while the fight is held, the
    // header keeps naming the dungeon it was fought in, not whatever scene
    // is live now -- so `scene_id`/`scene_name` below stay `TOWERING_RUIN`
    // even after the party is standing in `ASTERIA_PLAINS`. Asserted via
    // the golden, not a bare `assert_eq!`, so a change here is reviewed
    // rather than silently accepted.
    assert_eq!(left.snapshot.encounter.scene_id, Some(TOWERING_RUIN));

    assert_golden(pre_entry);
    assert_golden(entered);
    assert_golden(ended);
    assert_golden(left);
}

/// Issue #342, scenario 8: two dungeons pulled back to back. The party
/// finishes a boss in `TOWERING_RUIN`, reconnects, and lands straight in a
/// different dungeon instance (`OTHER_DUNGEON`) — no open-world stop in
/// between. `ResetReason::SceneChanged` fires immediately on the `Scene`
/// event (the first pull already latched its own end on the kill, so
/// `cut_short` reads false and the fast reset in `encounter.rs`'s `Scene`
/// arm applies), and the new instance's own `DungeonState::Playing` signal
/// then confirms it (issue #295's real-world shape, covered at the
/// `bpsr-meter` unit level by
/// `a_new_dungeons_playing_signal_resets_a_fight_held_since_a_raid_selection_died`
/// — this is that same confirm-on-top-of-an-already-fired-reset shape, but
/// through the real wire/pipeline stack and ending on a boss kill rather
/// than a raid-selection death). Either signal alone would already leave
/// the new pull clean; both firing is what a real back-to-back queue
/// produces.
#[test]
fn back_to_back_dungeons() {
    let scenario = Scenario::new("back_to_back_dungeons")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        // See the module doc on was_tracked_boss/recompute_boss ordering.
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        .at(2_100)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 40_000).kill()])
        .tick()
        .capture("dungeon_a_finalized")
        // A long real-world gap, then the reconnect into a genuinely
        // different dungeon.
        .at(600_000)
        .inject(ProtocolEvent::ServerChanged)
        .at(600_500)
        .enter_scene(OTHER_DUNGEON)
        .at(601_000)
        .dungeon_state(OTHER_DUNGEON, DUNGEON_PLAYING)
        // Players/entities do not survive a reconnect (`ServerChanged`
        // clears `enemies`; `reset` clears `players`), so the new instance's
        // roster and boss both have to be (re-)announced.
        .at(601_500)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS_2, IGNISOR, 1_000_000, 1_000_000)
        .hit(P_ARIA, M_BOSS_2, 101, 25_000)
        .tick()
        .capture("dungeon_b_fresh_pull");

    let mut rig = Rig::new();
    let captures = rig.run(&scenario);
    assert_eq!(captures.len(), 2);
    let finalized = &captures[0];
    let fresh = &captures[1];

    assert_eq!(finalized.fight_state, FightState::Ended);
    assert!(finalized.resets.is_empty());
    assert_eq!(finalized.snapshot.total_damage, 90_000);
    assert_eq!(finalized.snapshot.rows.len(), 1);

    assert_eq!(
        fresh.resets,
        vec![
            (600_500, ResetReason::SceneChanged),
            (601_000, ResetReason::DungeonStarted),
        ],
        "the scene transition fires the fast reset, and the new dungeon's \
         own Playing signal confirms it on top"
    );
    assert_eq!(fresh.fight_state, FightState::Active);
    assert_eq!(
        fresh.snapshot.total_damage, 25_000,
        "the new instance's pull must not carry over the finalized 90_000"
    );
    assert_eq!(fresh.snapshot.rows.len(), 1);

    assert_golden(finalized);
    assert_golden(fresh);
}

/// Issue #342, scenario 9(a): the app closing — the whole `Pipeline` (and
/// its `HistoryHandle`) dropped, exactly as `main.rs`'s process exit does,
/// with no history-flushing shutdown call anywhere on that path (unlike
/// `Pipeline::shutdown_names_cache`, which the name cache does get), using
/// `Rig::with_history` and a real temp-file SQLite store the same way
/// `replay_history.rs` does.
///
/// A fight that already finished and cleared its post-end grace window —
/// already sent to the history channel — survives an abrupt drop happening
/// mid the *next*, still-unfinished pull without panicking: the channel is
/// unbounded (`crates/app/src/history/writer.rs`), so the writer thread
/// drains everything already queued before `rx.recv` finally errs and the
/// thread exits; the still-active second pull is correctly never recorded
/// at all, because it never finished.
#[test]
fn app_shutdown_mid_next_pull_keeps_flushed_fight() {
    let path_a = std::env::temp_dir().join(format!(
        "shinra-shutdown-mid-fight-safe-{}.sqlite",
        std::process::id()
    ));
    let _guard_a = TempDb(path_a.clone());
    let _ = std::fs::remove_file(&path_a);

    let scenario_a = Scenario::new("app_shutdown_mid_fight_prior_survives")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        // See the module doc on was_tracked_boss/recompute_boss ordering.
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // `RetentionPolicy::default().min_duration_ms` is 5_000ms — clear
        // of the floor, like `replay_history.rs`'s `boss_kill_scenario`.
        .at(8_000)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 40_000).kill()])
        // Past `post_end_grace_ms` (2_000ms default): `record_fight_end`
        // actually flushes fight one's record to the history channel here.
        .at(10_500)
        .tick()
        // A second pull starts on a fresh boss uid and is still running --
        // no kill, no wipe, no idle timeout, no dungeon end -- when the app
        // "closes".
        .at(11_000)
        .monster_appear(M_BOSS_2, IGNISOR, 1_000_000, 1_000_000)
        .hit(P_ARIA, M_BOSS_2, 101, 50_000);

    let (mut rig, thread) = Rig::new().with_history(path_a.clone(), RetentionPolicy::default());
    rig.run(&scenario_a);
    // `Rig::fight_state` only updates on a `tick` (mirroring production's
    // 10Hz publish loop) — this is that last tick, observing the second
    // pull still `Active` the instant before the app "closes".
    assert_eq!(
        rig.tick(11_500),
        FightState::Active,
        "sanity check: the second pull is still running when the app closes"
    );

    // The shutdown: no explicit flush call anywhere, just drop -- the same
    // as the process exiting mid-pull. Must not panic.
    drop(rig);
    thread
        .join()
        .expect("history writer thread must not panic on abrupt pipeline drop");

    let store =
        SqliteHistory::open(&path_a, RetentionPolicy::default()).expect("reopen the history store");
    let rows = store.list(50).expect("list encounters");
    assert_eq!(
        rows.len(),
        1,
        "only the first, already-flushed-past-grace fight is in history; \
         the still-active second pull was correctly never recorded -- it \
         never finished"
    );
    assert_eq!(rows[0].total_damage, 90_000);
}

/// Issue #342, scenario 9(b): a real gap this test pins deliberately, not
/// by accident. `Pipeline::record_fight_end` only ever sends a fight's
/// `pending_fight_end` to history from a *later* tick/step call — once the
/// post-end grace window has closed, or the state leaves `Ended` early
/// (`crates/app/src/pipeline.rs`'s `record_fight_end`/
/// `settle_pending_fight_end`). There is no `Drop for Pipeline` and nothing
/// on this shutdown path flushes a still-pending record. A fight that ends
/// and the app closes again before either of those fires — well inside
/// `post_end_grace_ms` — never reaches the history channel at all: its
/// whole record is silently lost. See this PR's "Deviations / follow-ups".
#[test]
#[ignore = "known gap: pending_fight_end is not flushed when Pipeline is dropped inside post_end_grace_ms; see #342 follow-ups"]
fn app_shutdown_inside_grace_window_loses_the_record() {
    // The gap: a fight that ends and the app closes again before a later
    // tick has closed the grace window over it.
    let path_b = std::env::temp_dir().join(format!(
        "shinra-shutdown-mid-fight-grace-gap-{}.sqlite",
        std::process::id()
    ));
    let _guard_b = TempDb(path_b.clone());
    let _ = std::fs::remove_file(&path_b);

    let scenario_b = Scenario::new("app_shutdown_inside_grace_window_loses_the_record")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        // See the module doc on was_tracked_boss/recompute_boss ordering.
        .hit(P_ARIA, M_BOSS, 101, 50_000)
        // Clear of `min_duration_ms`'s 5_000ms floor, same as scenario (a).
        .at(8_000)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 40_000).kill()])
        // Still well inside the 2_000ms grace window: `record_fight_end`
        // only builds and caches `pending_fight_end` here, it does not
        // send it -- that needs a *later* tick this scenario never makes.
        .at(8_500)
        .tick();

    let (mut rig, thread) = Rig::new().with_history(path_b.clone(), RetentionPolicy::default());
    rig.run(&scenario_b);
    assert_eq!(rig.fight_state(), FightState::Ended);

    drop(rig);
    thread
        .join()
        .expect("history writer thread must not panic on abrupt pipeline drop");

    let store =
        SqliteHistory::open(&path_b, RetentionPolicy::default()).expect("reopen the history store");
    let rows = store.list(50).expect("list encounters");
    assert_eq!(
        rows.len(),
        1,
        "desired behaviour: a fight that ends inside its own post-end grace \
         window must still be flushed to history when the app closes, \
         instead of being silently lost"
    );
    assert_eq!(rows[0].total_damage, 90_000);
}
