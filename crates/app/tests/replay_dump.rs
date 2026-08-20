//! System test for the replay harness (`docs/replay-system-tests.md`),
//! driven from a *real*, sanitized packet capture rather than a hand-built
//! `Scenario` — the synthetic scenarios in `replay_pull.rs`/
//! `replay_lifecycle.rs` only prove the pipeline behaves consistently with
//! our own understanding of the protocol; this proves it against actual
//! wire traffic (see `docs/replay-system-tests.md` for the sanitizer, the
//! fixture's provenance, and its known coverage gaps).
//!
//! The fixture (`tests/fixtures/dump-2976-boss-fight.jsonl.zst`) is a ~209s
//! window (`--since 1787022118000 --until 1787022327000`) of one real boss
//! fight: monster id 1152 ("Kartgriff", a recognized boss) in scene id 8
//! ("Asterleeds"). It has been sanitized
//! (`crates/protocol/src/bin/sanitize-dump.rs`): uids remapped, names
//! replaced.
//!
//! `--since` was chosen with ~15s of margin before Kartgriff's *only*
//! `MONSTER_ID` sighting in the raw dump (`ts=1787022133362`) — identity
//! attrs are sent once, on first appear, not on every HP delta (see
//! `docs/replay-system-tests.md`), so starting any later would have lost
//! it, same as the previous (bad) fixture window did. `--until` was pushed
//! ~14s past the old window's end to also catch the raw dump's *only*
//! `EnterScene` record and its *only* `SyncContainerData` (player identity)
//! record, both at `ts=1787022325888` — a lucky pair, since neither opcode
//! fires more than once in the whole capture. That packet resolves one
//! player's real name/class/ability_score (remapped to `Player100005`,
//! `Class::ShieldKnight`, ability_score 62186); the other four players in
//! this fight never got a `SyncContainerData` broadcast in the raw capture
//! (the game only sends it for the local player, not party members), so
//! their rows stay `Player <uid>` with `class = None`.
//!
//! This window's only *damaged* entities are Kartgriff and the five
//! players — an earlier, unrelated trash pull by the same party (monster
//! ids 1341/1344/1358/1362, all non-boss) ends at `ts=1787022112670`, well
//! before `--since`, specifically to keep this fixture a single clean
//! encounter rather than splicing two fights together. So there is no
//! non-boss monster with a resolvable `monster_id` taking real damage in
//! this window to assert `boss_monster_id` against — see the
//! `boss_monster_id`/`is_boss` assertions below for how the fix is proven
//! instead: the encounter title survives the presence of Kartgriff's own
//! adds (a `1153`/"Illusory Bloom" trio and a `1151` alt-id, both spawning
//! alongside it at `ts=1787022133362`) without needing to name-check them,
//! because none of them take damage to compete for `boss_uid` in the first
//! place.
//!
//! Dump records are already post-zstd and post-frame-split (each payload is
//! one Notify body), so this test enters at `common::Rig::feed_notify`
//! (`decode_notify -> Pipeline::step`), not `Rig::run`'s TCP-byte seam.

mod common;

use std::path::Path;

use bpsr_protocol::dump_format;
use common::{Capture, Rig, assert_golden};

/// The real boss this fixture's window is built around. The single most
/// valuable thing this test proves: the encounter title resolves to the
/// real boss — asserted directly below, not merely implicitly via the
/// golden (a regenerated golden would silently accept a regression here).
///
/// There is deliberately no `NON_BOSS_MONSTER_IDS` constant here: this
/// window's only damaged entities are Kartgriff, its own adds (which never
/// take damage), and the five players (see the module doc comment). No
/// non-boss monster with a resolvable `monster_id` takes real damage in
/// this window, so an `assert_ne!(boss_monster_id, Some(some_trash_id))`
/// here would be vacuous — it would pass whether or not the resolution
/// logic actually preferred the boss, since no non-boss candidate is ever
/// in the running. `boss_monster_id == Some(KARTGRIFF_MONSTER_ID)` plus
/// `is_boss == true` below are the real, positive proof.
const KARTGRIFF_MONSTER_ID: u32 = 1152;
const KARTGRIFF_NAME: &str = "Kartgriff";

/// The one player this window's single `SyncContainerData` packet
/// identifies (see the module doc comment) — remapped uid, name, class,
/// and ability_score, all read straight off the sanitized fixture.
const IDENTIFIED_PLAYER_UID: i64 = 100_005;
const IDENTIFIED_PLAYER_NAME: &str = "Player100005";
const IDENTIFIED_PLAYER_ABILITY_SCORE: u32 = 62_186;

/// `Meter`'s default idle timeout (`FightConfig::default().idle_timeout_ms`,
/// `crates/meter/src/fight.rs`) is 9s. This margin, added past the fixture's
/// last record, guarantees the fight has been forced to `Ended` by the time
/// the final snapshot is taken, regardless of how much quiet trailing time
/// the captured window itself happens to contain.
const POST_FIXTURE_TICK_MARGIN_MS: u64 = 20_000;

#[test]
fn replay_dump_boss_fight() {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dump-2976-boss-fight.jsonl.zst"
    ));
    let compressed = std::fs::read(path).expect("read fixture");
    let jsonl = zstd::stream::decode_all(compressed.as_slice()).expect("decompress fixture");
    let jsonl = String::from_utf8(jsonl).expect("fixture must be valid UTF-8 JSONL");

    let mut records = Vec::new();
    for (lineno, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match dump_format::parse_record(line) {
            Ok(record) => records.push(record),
            Err(err) => panic!("fixture line {}: malformed record: {err}", lineno + 1),
        }
    }
    assert!(!records.is_empty(), "fixture must contain records");

    let mut rig = Rig::new();
    let mut last_ts = 0u64;
    for record in &records {
        // A record the original capture couldn't decompress carries raw
        // compressed bytes, not protobuf — decode_notify would just fail to
        // parse it; skip it like a real decoder would (see
        // `DumpRecord::payload_decoded`'s doc comment).
        if !record.payload_decoded {
            continue;
        }
        rig.feed_notify(record.method_id, &record.payload, record.ts_ms);
        rig.tick(record.ts_ms);
        last_ts = record.ts_ms;
    }
    assert!(
        last_ts > 0,
        "fixture must contain at least one decoded record"
    );

    // Force the fight to `Ended` regardless of how much quiet trailing time
    // the captured window itself contains, then take the fight-end snapshot.
    let final_ts = last_ts + POST_FIXTURE_TICK_MARGIN_MS;
    let fight_state = rig.tick(final_ts);
    let snapshot = rig.snapshot(final_ts);

    // --- headline assertion: real boss resolves correctly ------------------
    assert_eq!(
        snapshot.encounter.boss_monster_id,
        Some(KARTGRIFF_MONSTER_ID),
        "encounter title must resolve to the real boss (Kartgriff, 1152)"
    );
    assert_eq!(snapshot.encounter.boss_name, Some(KARTGRIFF_NAME));
    assert!(
        snapshot.encounter.is_boss,
        "1152 is a recognized boss; is_boss must be true"
    );
    // The fixture's window now includes the raw dump's only `EnterScene`
    // record (see the module doc comment) — scene id/name resolve for real.
    assert_eq!(snapshot.encounter.scene_id, Some(8));
    assert_eq!(snapshot.encounter.scene_name, Some("Asterleeds"));

    // --- plausible aggregate shape -----------------------------------------
    assert_eq!(
        snapshot.rows.len(),
        5,
        "fixture has 5 players; got rows: {:?}",
        snapshot.rows.iter().map(|r| r.uid).collect::<Vec<_>>()
    );
    assert!(
        snapshot.total_damage > 0,
        "total_damage must be non-zero for a real boss fight"
    );
    assert!(
        snapshot.total_dps > 0.0,
        "total_dps must be non-zero for a real boss fight"
    );
    // The captured window is ~209s (--since 1787022118000 --until
    // 1787022327000); the fight's own damage-bearing duration should be a
    // large majority of that, not a sliver.
    assert!(
        (100_000..=200_000).contains(&snapshot.duration_ms),
        "fight duration {}ms is not a plausible ~148s/~209s-window fight",
        snapshot.duration_ms
    );

    // --- the one identified player -----------------------------------------
    // This window's only `SyncContainerData` packet identifies exactly one
    // player (see the module doc comment); the other four rows stay
    // `Player <uid>` with `class = None`, which is why this asserts on one
    // row specifically rather than on all of `rows`.
    let identified = snapshot
        .rows
        .iter()
        .find(|r| r.uid == IDENTIFIED_PLAYER_UID)
        .unwrap_or_else(|| {
            panic!(
                "expected a row for uid {IDENTIFIED_PLAYER_UID}; got uids: {:?}",
                snapshot.rows.iter().map(|r| r.uid).collect::<Vec<_>>()
            )
        });
    assert_eq!(identified.name, IDENTIFIED_PLAYER_NAME);
    assert_eq!(identified.class, Some(bpsr_meter::Class::ShieldKnight));
    assert_eq!(
        identified.ability_score,
        Some(IDENTIFIED_PLAYER_ABILITY_SCORE)
    );

    let capture = Capture {
        label: "replay_dump_boss_fight",
        at_ms: final_ts,
        snapshot,
        fight_state,
        resets: rig.resets(),
    };
    assert_golden(&capture);
}
