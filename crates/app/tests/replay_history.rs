//! System tests for the persistent encounter-history feature (issue #39,
//! `docs/plans/system-test-harness.md`): the round trip from scripted wire
//! bytes, through `TcpReassembler`/`Decoder`/`Pipeline`, across the
//! `Active -> Ended` edge trigger (`Pipeline::record_fight_end`), onto the
//! history thread's `HistoryHandle`, into a real (temp-file) SQLite
//! database, and back out again as an `EncounterRecord`/`EncounterSummary`
//! a caller can read.
//!
//! Every timestamp here is a scripted `at_ms`/`now_ms` value driven through
//! `Rig`/`Scenario` — never wall-clock — so a recorded `ended_at_ms` and the
//! retention math built on it stay exactly reproducible.

mod common;

use bpsr_app::history::sqlite::SqliteHistory;
use bpsr_app::history::writer::{HistoryEvent, HistoryHandle};
use bpsr_app::history::{EncounterRecord, EncounterSummary, HistoryStore, RetentionPolicy};
use bpsr_test_support::scenario::{Hit, Scenario};
use bpsr_test_support::wire::prof;
use common::{Rig, assert_history_golden};

const P_ARIA: i64 = 1001;
const P_BRIN: i64 = 1002;
const M_BOSS: i64 = 2001;
const M_BOSS_2: i64 = 2002;
const IGNISOR: u32 = 103;
const TOWERING_RUIN: u32 = 1101;
/// An open-world zone (`tables::is_dungeon_scene` is false for it), where
/// the idle timeout is the only thing that ends a pull — mirrors
/// `replay_lifecycle.rs`'s `ASTERIA_PLAINS`.
const ASTERIA_PLAINS: u32 = 7;

/// A fresh temp-file DB path per test, removed before and after use so
/// re-runs never see a stale file and CI never accumulates them.
fn temp_db_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "shinra-history-{}-{}.sqlite",
        label,
        std::process::id()
    ))
}

/// Requests the newest `limit` encounters over `history`'s reply channel and
/// blocks for the answer, panicking on anything but `Listed`.
fn list_all(history: &HistoryHandle) -> Vec<EncounterSummary> {
    let (reply_tx, reply_rx) = crossbeam_channel::unbounded();
    history.list(50, &reply_tx);
    match reply_rx.recv().unwrap() {
        HistoryEvent::Listed(rows) => rows,
        other => panic!("expected Listed, got {other:?}"),
    }
}

/// Requests one encounter's full detail over `history`'s reply channel and
/// blocks for the answer, panicking on anything but `Loaded`.
fn load_one(history: &HistoryHandle, id: i64) -> EncounterRecord {
    let (reply_tx, reply_rx) = crossbeam_channel::unbounded();
    history.load(id, &reply_tx);
    match reply_rx.recv().unwrap() {
        HistoryEvent::Loaded { record, .. } => *record,
        other => panic!("expected Loaded, got {other:?}"),
    }
}

/// A two-player boss pull that ends on a boss kill: hit, hit, a killing hit,
/// then a tick to observe the `Active -> Ended` edge and record it.
/// Duration is 6s (2_000 -> 8_000), clear of the default 5s floor.
///
/// The tick lands at `kill + 2_500`, not right after the kill: issue
/// #post-end-grace's `Pipeline::record_fight_end` no longer sends to
/// history on the very first `Ended` tick, only once
/// `FightConfig::post_end_grace_ms` (2_000ms by default) has fully elapsed
/// past the kill — a tick inside that window would build the record but
/// leave it queued (`Pipeline::pending_fight_end`), unsent until some later
/// tick flushes it. `ended_at_ms` and every recorded stat are still pinned
/// to the kill itself (`fight_end_ms`, frozen the instant the boss died),
/// so moving this later changes nothing about what gets recorded — only
/// when.
fn boss_kill_scenario(name: &'static str) -> Scenario {
    Scenario::new(name)
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 400_000)
        .at(8_000)
        .hits(M_BOSS, vec![Hit::new(P_BRIN, 202, 600_000).kill()])
        .at(10_500)
        .tick()
}

/// Two consecutive boss pulls in the same dungeon, each ending on a kill:
/// fight one ends at 8_000, fight two (a fresh boss uid, after the
/// `ResetReason::NewFight` its first post-kill hit triggers) ends at
/// 27_000. The `.at(22_000).tick()` in between is load-bearing: it observes
/// `FightState::Active` for the second pull, which is what clears
/// `Pipeline::record_fight_end`'s write-exactly-once latch before the
/// second kill — without it, the latch set by the first fight's `Ended`
/// tick would still read `true` and swallow the second recording. Real
/// production ticks every publish interval, so this interleaving always
/// happens there; the harness has to script it explicitly.
///
/// Like `boss_kill_scenario`, the final tick lands at `kill + 2_500` so it
/// lands past the post-end grace window and the second fight's record
/// actually gets sent rather than left queued.
fn two_fights_scenario(name: &'static str) -> Scenario {
    boss_kill_scenario(name)
        .at(20_000)
        .monster_appear(M_BOSS_2, IGNISOR, 1_000_000, 1_000_000)
        .at(21_000)
        .hit(P_ARIA, M_BOSS_2, 101, 300_000)
        .at(22_000)
        .tick()
        .at(27_000)
        .hits(M_BOSS_2, vec![Hit::new(P_ARIA, 101, 700_000).kill()])
        .at(29_500)
        .tick()
}

/// A finished boss pull lands a row in the database: run the scenario,
/// drop the `Rig` (closing the `HistoryHandle`), join the history thread so
/// its write has definitely landed, then reopen the store fresh — proving
/// this is a real on-disk round trip, not just an in-memory echo of the
/// live handle.
#[test]
fn a_finished_boss_fight_lands_in_the_database() {
    let path = temp_db_path("boss-kill");
    let _ = std::fs::remove_file(&path);

    let scenario = boss_kill_scenario("history_boss_kill_scenario");

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);
    drop(rig);
    let _ = history_thread.join();

    let store =
        SqliteHistory::open(&path, RetentionPolicy::default()).expect("reopen the history store");
    let encounters = store.list(50).expect("list encounters");
    assert_eq!(encounters.len(), 1);
    let record = store
        .load(encounters[0].id)
        .expect("load encounter")
        .expect("encounter must exist");

    assert_eq!(record.meter_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(record.boss_monster_id, Some(IGNISOR));
    assert!(record.is_boss);
    assert_eq!(record.ended_at_ms, 8_000);

    assert_history_golden("history_boss_kill", &encounters, &record);

    let _ = std::fs::remove_file(&path);
}

/// `Pipeline::record_fight_end`'s write-exactly-once latch, exercised
/// through the harness rather than unit-tested directly: a dozen extra
/// ticks across the post-fight freeze must not produce a dozen rows.
#[test]
fn the_same_fight_is_never_recorded_twice() {
    let path = temp_db_path("dedup");
    let _ = std::fs::remove_file(&path);

    let mut scenario = boss_kill_scenario("history_dedup_scenario");
    for at_ms in (9_000..=20_000).step_by(1_000) {
        scenario = scenario.at(at_ms).tick();
    }

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert_eq!(encounters.len(), 1);

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);
}

/// DECISION D3: an idle-timeout end records the *last hit's* timestamp as
/// `ended_at_ms`, not the tick that happened to observe the freeze.
///
/// Issue #313 widened the engagement-window hold (`BOSS_ENGAGEMENT_WINDOW_MS`,
/// 60s) to every scene, not just dungeon instances: `IGNISOR` here is a
/// recognized boss (`tables::is_boss_monster`) that has taken damage, in the
/// open-world `ASTERIA_PLAINS` scene, so the idle timeout is only allowed to
/// bite once that window lapses past the last hit — the tick below has to
/// wait for both.
#[test]
fn an_idle_timeout_end_records_the_last_hit_as_the_end_time() {
    let path = temp_db_path("idle-end-time");
    let _ = std::fs::remove_file(&path);

    let scenario = Scenario::new("history_idle_end_scenario")
        .at(1_000)
        .enter_scene(ASTERIA_PLAINS)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 300_000)
        .at(9_000)
        .hit(P_ARIA, M_BOSS, 101, 300_000)
        // Last damage (9_000) + BOSS_ENGAGEMENT_WINDOW_MS 60_000 (the
        // engagement-window guard, issue #313) + idle_timeout_ms 9_000,
        // plus a 1s margin so the tick lands past the boundary rather than
        // exactly on it.
        .at(79_000)
        .tick();

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert_eq!(encounters.len(), 1);
    let record = load_one(
        rig.history().expect("history handle attached"),
        encounters[0].id,
    );

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        record.ended_at_ms, 9_000,
        "the end time must be the last hit, not the observing tick"
    );
}

/// The 5s duration floor (`RetentionPolicy::default().min_duration_ms`)
/// rejects a pull that never got going.
#[test]
fn a_short_pull_is_not_recorded() {
    let path = temp_db_path("short-pull");
    let _ = std::fs::remove_file(&path);

    let scenario = Scenario::new("history_short_pull_scenario")
        .at(1_000)
        .enter_scene(ASTERIA_PLAINS)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .monster_appear(M_BOSS, IGNISOR, 5_000_000, 5_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 100_000)
        .at(3_000)
        .hit(P_ARIA, M_BOSS, 101, 100_000)
        // duration_ms = 3_000 - 2_000 = 1_000, under the 5_000 floor.
        .at(12_000)
        .tick();

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert!(encounters.is_empty());

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);
}

/// The saved record's player rows must match the live `Snapshot`'s rows
/// field-for-field — the whole point of `EncounterRecord::to_snapshot`
/// rebuilding through the live table path.
#[test]
fn the_recorded_rows_match_the_live_snapshot() {
    let path = temp_db_path("rows-match");
    let _ = std::fs::remove_file(&path);

    let scenario = Scenario::new("history_rows_scenario")
        .at(1_000)
        .enter_scene(TOWERING_RUIN)
        .player_appear(P_ARIA, "Aria", prof::STORMBLADE, 12_000)
        .player_appear(P_BRIN, "Brin", prof::FROST_MAGE, 11_500)
        .monster_appear(M_BOSS, IGNISOR, 1_000_000, 1_000_000)
        .at(2_000)
        .hit(P_ARIA, M_BOSS, 101, 300_000)
        .at(3_000)
        .hit(P_BRIN, M_BOSS, 202, 200_000)
        .at(9_000)
        .hits(M_BOSS, vec![Hit::new(P_ARIA, 101, 500_000).kill()])
        // kill + 2_500, past the post-end grace window — see
        // `boss_kill_scenario`'s doc comment.
        .at(11_500)
        .tick();

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);
    let live_snapshot = rig.snapshot(9_500);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert_eq!(encounters.len(), 1);
    let record = load_one(
        rig.history().expect("history handle attached"),
        encounters[0].id,
    );

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);

    let rebuilt = record.to_snapshot();
    let mut live_rows: Vec<_> = live_snapshot.rows.iter().collect();
    live_rows.sort_by_key(|r| r.uid);
    let mut saved_rows: Vec<_> = rebuilt.rows.iter().collect();
    saved_rows.sort_by_key(|r| r.uid);

    assert_eq!(live_rows.len(), saved_rows.len());
    for (live, saved) in live_rows.iter().zip(saved_rows.iter()) {
        assert_eq!(live.uid, saved.uid);
        assert_eq!(live.name, saved.name);
        assert_eq!(live.class, saved.class);
        assert_eq!(live.ability_score, saved.ability_score);
        assert_eq!(live.season_strength, saved.season_strength);
        assert_eq!(live.imagines, saved.imagines);
        assert_eq!(live.imagine_tiers, saved.imagine_tiers);
        assert_eq!(live.damage, saved.damage);
        assert_eq!(live.dps, saved.dps);
        assert_eq!(live.share_pct, saved.share_pct);
        assert_eq!(live.crit_pct, saved.crit_pct);
        assert_eq!(live.lucky_pct, saved.lucky_pct);
        assert_eq!(live.hits, saved.hits);
        assert_eq!(live.deaths, saved.deaths);
    }

    assert_history_golden("history_rows", &encounters, &record);
}

/// `list()` replies newest first, across two genuinely separate recorded
/// fights in the same replay.
#[test]
fn two_fights_are_listed_newest_first() {
    let path = temp_db_path("two-fights");
    let _ = std::fs::remove_file(&path);

    let scenario = two_fights_scenario("history_two_fights_scenario");

    let (mut rig, history_thread) =
        Rig::new().with_history(path.clone(), RetentionPolicy::default());
    rig.run(&scenario);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert_eq!(encounters.len(), 2);
    assert_eq!(encounters[0].ended_at_ms, 27_000, "newest fight first");
    assert_eq!(encounters[1].ended_at_ms, 8_000, "oldest fight last");

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);
}

/// A `RetentionPolicy` with `max_encounters: 1` prunes down to the newest
/// row after each insert, so replaying two fights leaves exactly the
/// second one.
#[test]
fn the_count_cap_prunes_across_a_replay() {
    let path = temp_db_path("count-cap");
    let _ = std::fs::remove_file(&path);

    let scenario = two_fights_scenario("history_count_cap_scenario");
    let policy = RetentionPolicy {
        max_encounters: 1,
        ..RetentionPolicy::default()
    };

    let (mut rig, history_thread) = Rig::new().with_history(path.clone(), policy);
    rig.run(&scenario);

    let encounters = list_all(rig.history().expect("history handle attached"));
    assert_eq!(encounters.len(), 1);
    assert_eq!(
        encounters[0].ended_at_ms, 27_000,
        "the surviving row must be the second (newer) fight"
    );

    drop(rig);
    let _ = history_thread.join();
    let _ = std::fs::remove_file(&path);
}
