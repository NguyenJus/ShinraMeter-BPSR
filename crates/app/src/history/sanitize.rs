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
//! over the same source produce the same mapping (first-seen order is
//! deterministic for a given source file).
//!
//! `encounters.boss_name`/`scene_name`/`title`/`subtitle` are left
//! untouched: they name monsters and zones, not players, and `sanitize-dump`
//! draws the same line (it only remaps uids and the `SyncNearEntities`/
//! `AoiSyncDelta` player name attr, never monster names).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use rusqlite::Connection;

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
    /// `Player<n>`, matching `sanitize-dump`'s `name_for` exactly, so a
    /// name seen in a sanitized dump and a sanitized history export for the
    /// same session reads as the same player.
    fn name_for(&mut self, old_uid: i64) -> String {
        format!("Player{}", self.uid(old_uid))
    }
}

/// Copies the history database at `src` to `dst`, then rewrites every
/// `encounter_players` row's `uid` and `name` in the copy to a stable
/// pseudonym pair (see [`Remap`]) — the same uid always yields the same
/// pseudonym within the copy, so a reader can still tell two rows are the
/// same player without learning who that player is.
///
/// `src` is copied byte-for-byte first (cheap, and means the rewrite below
/// only ever touches `dst` — `src`, e.g. the live `history.sqlite`, is never
/// opened for writing). Every other column — `encounters.boss_name`,
/// `scene_name`, `title`, `subtitle`, all of `encounter_player_skills` — is
/// left exactly as copied: none of them name a player.
pub fn sanitize_copy(src: &Path, dst: &Path) -> Result<SanitizeReport, HistoryError> {
    crate::paths::ensure_parent_dir(dst).map_err(HistoryError::Copy)?;
    fs::copy(src, dst).map_err(HistoryError::Copy)?;

    let conn = Connection::open(dst)?;
    let mut report = SanitizeReport::default();

    let encounters: i64 =
        conn.query_row("SELECT COUNT(*) FROM encounters", [], |row| row.get(0))?;
    report.encounters = encounters.max(0) as u64;

    let rows: Vec<(i64, i64, i64)> = {
        let mut stmt = conn.prepare("SELECT encounter_id, slot, uid FROM encounter_players")?;
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
    for (encounter_id, slot, uid) in rows {
        let new_uid = remap.uid(uid);
        let new_name = remap.name_for(uid);
        conn.execute(
            "UPDATE encounter_players SET uid = ?1, name = ?2 WHERE encounter_id = ?3 AND slot = ?4",
            rusqlite::params![new_uid, new_name, encounter_id, slot],
        )?;
        report.players_remapped += 1;
    }

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
}
