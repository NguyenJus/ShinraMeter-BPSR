//! Sanitizes a copy of `history.sqlite` for sharing (issue #347): rewrites
//! every player-identifying column to a stable pseudonym so the copy can
//! ride along in a session bundle (`crate::bundle`) without leaking real
//! player names.
//!
//! Reuses the pseudonym scheme `bpsr_protocol`'s `sanitize-dump` binary
//! already established for dump files (`sanitize::Remap` there): a
//! `uid -> uid` remap assigning small sequential ids, starting at
//! `100_000` (clear of any real uid range), in first-seen order — so the
//! same real player gets the same replacement uid *and* the same
//! `Player<n>` name everywhere in one sanitized copy, and repeated runs
//! over the same source produce the same mapping: rows are visited in
//! `(encounter_id, slot)` order, which is fixed by the source file's own
//! data rather than by anything nondeterministic like statement order or
//! disk layout, so the mapping is reproducible run to run.
//!
//! `encounters.boss_name`/`scene_name`/`title`/`subtitle` are left
//! untouched: they name monsters and zones, not players, and `sanitize-dump`
//! draws the same line (it only remaps uids and the `SyncNearEntities`/
//! `AoiSyncDelta` player name attr, never monster names).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::HistoryError;

/// What [`sanitize_copy`] did to `dst` — enough for a caller (the bundle
/// export) to log a one-line summary without re-querying the sanitized
/// copy itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SanitizeReport {
    /// Rows in `encounters` (unchanged by sanitizing — copied over as-is).
    pub encounters: u64,
    /// `encounter_players` rows whose `uid`/`name` were rewritten.
    pub players_remapped: u64,
}

/// A stable old-uid -> new-uid remap, assigning small sequential ids
/// (starting at 100_000, clear of any real uid range) in first-seen order
/// so the same player gets the same placeholder everywhere in one
/// sanitized copy. Field-for-field the same scheme as
/// `bpsr_protocol::bin::sanitize_dump::sanitize::Remap`, reimplemented here
/// rather than shared: that one lives in a `bin`, which cannot be
/// depended on as a library, and the scheme is a handful of lines.
struct Remap {
    uids: BTreeMap<i64, i64>,
    next: i64,
}

impl Remap {
    fn new() -> Self {
        Self {
            uids: BTreeMap::new(),
            next: 100_000,
        }
    }

    /// `0` is never a real player uid in this schema (`encounter_players.uid`
    /// is `NOT NULL` and always populated from a live roster entry) but is
    /// passed through unchanged anyway, defensively, rather than consuming
    /// a placeholder slot for it.
    fn uid(&mut self, old: i64) -> i64 {
        if old == 0 {
            return 0;
        }
        let n = self.next;
        let v = *self.uids.entry(old).or_insert(n);
        if v == n {
            self.next += 1;
        }
        v
    }

    /// The pseudonym for `old_uid`'s (already-remapped) replacement —
    /// `Player<n>`, matching `sanitize-dump`'s `name_for` exactly.
    fn name_for(&mut self, old_uid: i64) -> String {
        format!("Player{}", self.uid(old_uid))
    }
}

/// Snapshots the history database at `src` into `dst`, then rewrites every
/// `encounter_players` row's `uid` and `name` in the copy to a stable
/// pseudonym pair (see [`Remap`]) — the same uid always yields the same
/// pseudonym within the copy, so a reader can still tell two rows are the
/// same player without learning who that player is.
///
/// `src` is opened read-only and snapshotted with SQLite's own `VACUUM
/// INTO` rather than `fs::copy`: that's a consistent, transactionally-safe
/// snapshot straight from SQLite (no free pages carried over either) built
/// without ever taking a write lock on `src` — this is what makes it safe
/// to run against the live `history.sqlite` while the writer thread is
/// still active; a concurrent commit on `src` is covered by rusqlite's
/// default 5-second busy timeout. The rewrite then runs against `dst`
/// alone; `src` is never opened for writing. After the rewrite, `dst` gets
/// `secure_delete` turned on for the `UPDATE`s and a final `VACUUM`, so the
/// real names/uids the `UPDATE`s overwrote don't linger as stale bytes in
/// `dst`'s free pages. Every other column — `encounters.boss_name`,
/// `scene_name`, `title`, `subtitle`, all of `encounter_player_skills` — is
/// left exactly as copied: none of them name a player.
///
/// On any error, `dst` (and a `<dst>-journal` sibling, if SQLite left one
/// behind) is removed before returning — a half-sanitized copy must never
/// be mistaken for a finished one by a caller that only checks whether the
/// file exists.
pub fn sanitize_copy(src: &Path, dst: &Path) -> Result<SanitizeReport, HistoryError> {
    match sanitize_into(src, dst) {
        Ok(report) => Ok(report),
        Err(err) => {
            let _ = fs::remove_file(dst);
            let mut journal = dst.as_os_str().to_owned();
            journal.push("-journal");
            let _ = fs::remove_file(journal);
            Err(err)
        }
    }
}

/// Does the actual work of [`sanitize_copy`] — split out so that function
/// can wrap it in one place with the on-error cleanup every path through
/// this needs, rather than every early return remembering to clean up
/// after itself.
fn sanitize_into(src: &Path, dst: &Path) -> Result<SanitizeReport, HistoryError> {
    crate::paths::ensure_parent_dir(dst).map_err(HistoryError::Copy)?;
    // `VACUUM INTO` refuses to write over an existing file.
    let _ = fs::remove_file(dst);

    let dst_str = dst.to_str().ok_or_else(|| {
        HistoryError::Copy(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("destination path {} is not valid UTF-8", dst.display()),
        ))
    })?;

    {
        let src_conn = Connection::open_with_flags(
            src,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        src_conn.execute("VACUUM INTO ?1", [dst_str])?;
    }

    let mut conn = Connection::open(dst)?;
    conn.pragma_update(None, "secure_delete", "ON")?;

    let mut report = SanitizeReport::default();

    let encounters: i64 =
        conn.query_row("SELECT COUNT(*) FROM encounters", [], |row| row.get(0))?;
    report.encounters = encounters.max(0) as u64;

    let rows: Vec<(i64, i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT encounter_id, slot, uid FROM encounter_players ORDER BY encounter_id, slot",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let mut remap = Remap::new();
    {
        let tx = conn.transaction()?;
        for (encounter_id, slot, uid) in rows {
            let new_uid = remap.uid(uid);
            let new_name = remap.name_for(uid);
            tx.execute(
                "UPDATE encounter_players SET uid = ?1, name = ?2 WHERE encounter_id = ?3 AND slot = ?4",
                rusqlite::params![new_uid, new_name, encounter_id, slot],
            )?;
            report.players_remapped += 1;
        }
        tx.commit()?;
    }

    conn.execute_batch("VACUUM")?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use bpsr_meter::Class;

    use super::*;
    use crate::history::sqlite::SqliteHistory;
    use crate::history::{EncounterRecord, HistoryStore, PlayerRecord, RetentionPolicy};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-sanitize-test-{tag}-{}-{n}.sqlite",
            std::process::id()
        ))
    }

    fn sample_player(uid: i64, name: &str) -> PlayerRecord {
        PlayerRecord {
            uid,
            name: name.to_string(),
            class: Some(Class::FrostMage),
            ability_score: Some(999),
            season_strength: Some(42),
            imagines: [Some(1), None],
            imagine_tiers: [Some(3), None],
            damage: 5_000,
            dps: 500.0,
            share_pct: 33.3,
            crit_pct: 12.5,
            lucky_pct: 6.25,
            hits: 40,
            deaths: 2,
            skills: Vec::new(),
        }
    }

    fn sample_record(
        ended_at_ms: u64,
        duration_ms: u64,
        players: Vec<PlayerRecord>,
    ) -> EncounterRecord {
        EncounterRecord {
            ended_at_ms,
            duration_ms,
            total_damage: 10_000,
            total_dps: 1_000.0,
            boss_monster_id: Some(7),
            boss_name: Some("Boss".to_string()),
            is_boss: true,
            scene_id: Some(3),
            scene_name: Some("Scene".to_string()),
            title: "Boss".to_string(),
            subtitle: Some("Scene".to_string()),
            meter_version: "0.2.2".to_string(),
            players,
        }
    }

    /// Builds a real on-disk `history.sqlite` (via the same `SqliteHistory`
    /// the app writes with) at a fresh temp path, with two encounters:
    /// one player (uid 1, "Alice") repeats across both, and uid 2 ("Bob")
    /// appears only in the second — enough to exercise a name repeating
    /// across rows without every row sharing a uid.
    fn seed_history() -> std::path::PathBuf {
        let path = temp_db_path("seed");
        let mut store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap();
        store
            .insert(&sample_record(
                2_000,
                10_000,
                vec![sample_player(1, "Alice"), sample_player(2, "Bob")],
            ))
            .unwrap();
        path
    }

    fn all_names(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM encounter_players ORDER BY encounter_id, slot")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn all_uids(path: &Path) -> Vec<i64> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT uid FROM encounter_players ORDER BY encounter_id, slot")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn sanitize_copy_removes_every_real_name() {
        let src = seed_history();
        let dst = temp_db_path("out-names");

        sanitize_copy(&src, &dst).unwrap();

        let names = all_names(&dst);
        assert!(
            !names.iter().any(|n| n == "Alice" || n == "Bob"),
            "{names:?}"
        );
        assert!(names.iter().all(|n| n.starts_with("Player")), "{names:?}");

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    #[test]
    fn sanitize_copy_gives_the_same_uid_the_same_pseudonym_everywhere() {
        let src = seed_history();
        let dst = temp_db_path("out-stable");

        sanitize_copy(&src, &dst).unwrap();

        let names = all_names(&dst);
        // Row order: (enc1, Alice), (enc2, Alice), (enc2, Bob).
        assert_eq!(
            names[0], names[1],
            "Alice's pseudonym must be stable across rows"
        );
        assert_ne!(names[0], names[2], "distinct players must not collide");

        let uids = all_uids(&dst);
        assert_eq!(uids[0], uids[1]);
        assert_ne!(uids[0], uids[2]);

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    #[test]
    fn sanitize_copy_leaves_encounter_counts_and_damage_totals_unchanged() {
        let src = seed_history();
        let dst = temp_db_path("out-counts");

        let report = sanitize_copy(&src, &dst).unwrap();
        assert_eq!(report.encounters, 2);
        assert_eq!(report.players_remapped, 3);

        let src_store = SqliteHistory::open(&src, RetentionPolicy::default()).unwrap();
        let dst_store = SqliteHistory::open(&dst, RetentionPolicy::default()).unwrap();
        let src_list = src_store.list(10).unwrap();
        let dst_list = dst_store.list(10).unwrap();

        assert_eq!(src_list.len(), dst_list.len());
        for (a, b) in src_list.iter().zip(dst_list.iter()) {
            assert_eq!(a.total_damage, b.total_damage);
            assert_eq!(a.total_dps, b.total_dps);
            assert_eq!(a.duration_ms, b.duration_ms);
            assert_eq!(a.player_count, b.player_count);
        }

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    #[test]
    fn sanitize_copy_does_not_touch_the_source_file() {
        let src = seed_history();
        let dst = temp_db_path("out-src-untouched");

        sanitize_copy(&src, &dst).unwrap();

        assert_eq!(all_names(&src), vec!["Alice", "Alice", "Bob"]);

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    /// A corrupt/non-database `src` must not leave a half-written `dst`
    /// behind for a caller that only checks whether the file exists.
    #[test]
    fn sanitize_copy_leaves_no_dst_behind_on_a_corrupt_source() {
        let src = temp_db_path("corrupt-src");
        fs::write(&src, b"not a database").unwrap();
        let dst = temp_db_path("corrupt-dst");

        let result = sanitize_copy(&src, &dst);

        assert!(result.is_err(), "{result:?}");
        assert!(!dst.exists());

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    /// A deleted encounter's players must never survive into the sanitized
    /// copy — not as remapped rows, and not as leftover byte garbage in
    /// `dst`'s free pages, which `VACUUM`/`secure_delete` are what rule
    /// out.
    #[test]
    fn sanitize_copy_leaves_no_trace_of_a_deleted_players_name() {
        let src = temp_db_path("deleted-src");
        let mut store = SqliteHistory::open(&src, RetentionPolicy::default()).unwrap();
        store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap();
        store
            .insert(&sample_record(
                2_000,
                10_000,
                vec![sample_player(1, "Alice"), sample_player(2, "Bob")],
            ))
            .unwrap();
        let carol_id = store
            .insert(&sample_record(
                3_000,
                10_000,
                vec![sample_player(3, "Carol")],
            ))
            .unwrap()
            .unwrap();
        store.delete(carol_id).unwrap();
        drop(store);

        let dst = temp_db_path("deleted-dst");
        sanitize_copy(&src, &dst).unwrap();

        let bytes = fs::read(&dst).unwrap();
        for needle in [b"Alice".as_slice(), b"Bob".as_slice(), b"Carol".as_slice()] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "found {:?} in sanitized bytes",
                std::str::from_utf8(needle)
            );
        }

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    /// Two sanitized copies of the same source must assign the same
    /// pseudonyms in the same rows — the whole point of a *stable* remap.
    #[test]
    fn sanitize_copy_is_deterministic_across_runs_over_the_same_source() {
        let src = seed_history();
        let dst_a = temp_db_path("determinism-a");
        let dst_b = temp_db_path("determinism-b");

        sanitize_copy(&src, &dst_a).unwrap();
        sanitize_copy(&src, &dst_b).unwrap();

        assert_eq!(all_uids(&dst_a), all_uids(&dst_b));
        assert_eq!(all_names(&dst_a), all_names(&dst_b));

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst_a);
        let _ = fs::remove_file(&dst_b);
    }
}
