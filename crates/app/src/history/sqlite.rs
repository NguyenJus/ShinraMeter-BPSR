//! `SqliteHistory`: the [`super::HistoryStore`] implementation this build
//! ships (issue #39). The **only** file the spec §10 JSONL fallback would
//! replace — everything else in `crates/app/src/history` talks to the trait,
//! not to SQL.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use bpsr_meter::Class;

use super::{
    EncounterRecord, EncounterSummary, HistoryError, HistoryStore, PlayerRecord, RetentionPolicy,
    SCHEMA_VERSION,
};

/// Owns the single connection to `history.sqlite`. Never shared across
/// threads directly — WP2's history thread is the sole owner — which is why
/// this holds a plain `rusqlite::Connection` rather than anything
/// `Mutex`-wrapped.
pub struct SqliteHistory {
    conn: Connection,
    policy: RetentionPolicy,
}

impl SqliteHistory {
    /// Opens (creating if needed) the history database at `path`, applying
    /// the schema-version policy of spec §5.4: a fresh or already-current
    /// file is used as-is, and a file stamped with any other
    /// `PRAGMA user_version` is renamed aside (`<path>.v<n>.bak`) and
    /// replaced with a new empty schema rather than migrated (DECISION D11).
    /// Creates the parent directory if it doesn't exist yet.
    pub fn open(path: &Path, policy: RetentionPolicy) -> Result<Self, HistoryError> {
        Self::open_inner(path, policy, true)
    }

    /// `allow_reset` is the recursion guard mentioned in the plan: an
    /// unknown schema version renames the old file aside and retries exactly
    /// once with `allow_reset = false`, so a bug can never turn this into an
    /// infinite loop. A freshly (re)created file always reads
    /// `user_version == 0`, so in practice the retry always lands on the
    /// "create schema" branch below, never the unknown-version branch again.
    fn open_inner(
        path: &Path,
        policy: RetentionPolicy,
        allow_reset: bool,
    ) -> Result<Self, HistoryError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| HistoryError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                init_schema(&conn)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            v if v == SCHEMA_VERSION => {}
            v if allow_reset => {
                log::warn!(
                    "history db: unrecognized schema version {v} in {} (expected {SCHEMA_VERSION}); \
                     renaming it aside and starting fresh",
                    path.display()
                );
                drop(conn);
                let bak_path = path.with_extension(format!("v{v}.bak"));
                let _ = fs::remove_file(&bak_path);
                fs::rename(path, &bak_path).map_err(HistoryError::RenameAside)?;
                return Self::open_inner(path, policy, false);
            }
            v => {
                // Unreachable in practice (see the doc comment above), but
                // treated as "needs a fresh schema" rather than looping or
                // erroring, so a future bug here degrades instead of panics.
                log::warn!(
                    "history db: schema version {v} persisted across a reset attempt in {}; \
                     forcing a fresh schema",
                    path.display()
                );
                init_schema(&conn)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
        }

        Ok(Self { conn, policy })
    }

    /// Test seam: an in-memory database with the same schema, so the store's
    /// behaviour is testable with no filesystem at all.
    #[cfg(test)]
    fn in_memory(policy: RetentionPolicy) -> Result<Self, HistoryError> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        init_schema(&conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn, policy })
    }
}

/// The exact DDL of spec §5.3. `IF NOT EXISTS` so re-running it against an
/// already-current schema (the `v == SCHEMA_VERSION` branch skips this, but
/// nothing else should rely on that) is always a no-op rather than an error.
fn init_schema(conn: &Connection) -> Result<(), HistoryError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS encounters (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            ended_at_ms     INTEGER NOT NULL,
            duration_ms     INTEGER NOT NULL,
            total_damage    INTEGER NOT NULL,
            total_dps       REAL    NOT NULL,
            boss_monster_id INTEGER,
            boss_name       TEXT,
            is_boss         INTEGER NOT NULL,
            scene_id        INTEGER,
            scene_name      TEXT,
            title           TEXT    NOT NULL,
            subtitle        TEXT,
            player_count    INTEGER NOT NULL,
            meter_version   TEXT    NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_encounters_ended_at ON encounters(ended_at_ms DESC);
        CREATE TABLE IF NOT EXISTS encounter_players (
            encounter_id    INTEGER NOT NULL REFERENCES encounters(id) ON DELETE CASCADE,
            slot            INTEGER NOT NULL,
            uid             INTEGER NOT NULL,
            name            TEXT    NOT NULL,
            class           TEXT,
            ability_score   INTEGER,
            season_strength INTEGER,
            imagine_0       INTEGER,
            imagine_1       INTEGER,
            imagine_tier_0  INTEGER,
            imagine_tier_1  INTEGER,
            damage          INTEGER NOT NULL,
            dps             REAL    NOT NULL,
            share_pct       REAL    NOT NULL,
            crit_pct        REAL    NOT NULL,
            lucky_pct       REAL    NOT NULL,
            hits            INTEGER NOT NULL,
            deaths          INTEGER NOT NULL,
            PRIMARY KEY (encounter_id, slot)
        );",
    )?;
    Ok(())
}

/// The reverse of `Class::name()` (`crates/meter/src/event.rs`), used to read
/// the `class` column back. Every one of the ten `Class` variants is spelled
/// out explicitly, mirroring `Class::name()`'s own exhaustive match arm for
/// arm — the two must be kept in lockstep by hand, since this function's
/// input is a database `&str`, not a `Class`, so the compiler cannot enforce
/// it the way an exhaustive `match` on `Class` itself would (the discipline
/// `Class::role` documents). Genuinely unrecognized text (a hand-edited file,
/// a future variant not yet added here) falls back to `None` rather than
/// erroring — an unrecognized class is a display-only degradation, not a
/// reason to fail loading the whole encounter.
fn class_from_name(name: &str) -> Option<Class> {
    match name {
        "Stormblade" => Some(Class::Stormblade),
        "FrostMage" => Some(Class::FrostMage),
        "TwinStriker" => Some(Class::TwinStriker),
        "WindKnight" => Some(Class::WindKnight),
        "VerdantOracle" => Some(Class::VerdantOracle),
        "HeavyGuardian" => Some(Class::HeavyGuardian),
        "Marksman" => Some(Class::Marksman),
        "ShieldKnight" => Some(Class::ShieldKnight),
        "BeatPerformer" => Some(Class::BeatPerformer),
        "Unknown" => Some(Class::Unknown),
        _ => None,
    }
}

impl HistoryStore for SqliteHistory {
    fn insert(&mut self, record: &EncounterRecord) -> Result<Option<i64>, HistoryError> {
        if record.duration_ms < self.policy.min_duration_ms {
            return Ok(None);
        }

        let tx = self.conn.transaction()?;

        tx.execute(
            "INSERT INTO encounters (
                ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                boss_name, is_boss, scene_id, scene_name, title, subtitle,
                player_count, meter_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                i64::try_from(record.ended_at_ms).unwrap_or(i64::MAX),
                i64::try_from(record.duration_ms).unwrap_or(i64::MAX),
                record.total_damage,
                record.total_dps,
                record.boss_monster_id.map(i64::from),
                record.boss_name,
                record.is_boss,
                record.scene_id.map(i64::from),
                record.scene_name,
                record.title,
                record.subtitle,
                i64::try_from(record.players.len()).unwrap_or(i64::MAX),
                record.meter_version,
            ],
        )?;
        let encounter_id = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            )?;
            for (slot, player) in record.players.iter().enumerate() {
                stmt.execute(params![
                    encounter_id,
                    i64::try_from(slot).unwrap_or(i64::MAX),
                    player.uid,
                    player.name,
                    player.class.map(|c| c.name()),
                    player.ability_score.map(i64::from),
                    player.season_strength.map(i64::from),
                    player.imagines[0],
                    player.imagines[1],
                    player.imagine_tiers[0],
                    player.imagine_tiers[1],
                    player.damage,
                    player.dps,
                    f64::from(player.share_pct),
                    f64::from(player.crit_pct),
                    f64::from(player.lucky_pct),
                    i64::try_from(player.hits).unwrap_or(i64::MAX),
                    i64::from(player.deaths),
                ])?;
            }
        }

        // Age prune (spec §5.5.3): anchored on this record's own end time,
        // not `SystemTime::now()`, so pruning stays deterministic and
        // replay-testable.
        if self.policy.max_age_days > 0 {
            let cutoff = i64::try_from(record.ended_at_ms).unwrap_or(i64::MAX)
                - i64::from(self.policy.max_age_days) * 86_400_000;
            tx.execute(
                "DELETE FROM encounters WHERE ended_at_ms < ?1",
                params![cutoff],
            )?;
        }

        // Count prune (spec §5.5.4). `ON DELETE CASCADE` (foreign_keys is ON
        // for this connection) removes the orphaned player rows.
        if self.policy.max_encounters > 0 {
            tx.execute(
                "DELETE FROM encounters WHERE id NOT IN (
                    SELECT id FROM encounters ORDER BY ended_at_ms DESC, id DESC LIMIT ?1
                )",
                params![i64::from(self.policy.max_encounters)],
            )?;
        }

        tx.commit()?;
        Ok(Some(encounter_id))
    }

    fn list(&self, limit: u32) -> Result<Vec<EncounterSummary>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ended_at_ms, duration_ms, total_damage, total_dps, title, subtitle, player_count
             FROM encounters ORDER BY ended_at_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::from(limit)], |row| {
            Ok(EncounterSummary {
                id: row.get(0)?,
                ended_at_ms: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                duration_ms: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                total_damage: row.get(3)?,
                total_dps: row.get(4)?,
                title: row.get(5)?,
                subtitle: row.get(6)?,
                player_count: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(HistoryError::from)
    }

    fn load(&self, id: i64) -> Result<Option<EncounterRecord>, HistoryError> {
        let base = self
            .conn
            .query_row(
                "SELECT ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                        boss_name, is_boss, scene_id, scene_name, title, subtitle, meter_version
                 FROM encounters WHERE id = ?1",
                params![id],
                |row| {
                    Ok(EncounterRecord {
                        ended_at_ms: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                        duration_ms: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                        total_damage: row.get(2)?,
                        total_dps: row.get(3)?,
                        boss_monster_id: row
                            .get::<_, Option<i64>>(4)?
                            .map(|v| u32::try_from(v).unwrap_or(0)),
                        boss_name: row.get(5)?,
                        is_boss: row.get(6)?,
                        scene_id: row
                            .get::<_, Option<i64>>(7)?
                            .map(|v| u32::try_from(v).unwrap_or(0)),
                        scene_name: row.get(8)?,
                        title: row.get(9)?,
                        subtitle: row.get(10)?,
                        meter_version: row.get(11)?,
                        players: Vec::new(),
                    })
                },
            )
            .optional()?;

        let Some(mut record) = base else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT uid, name, class, ability_score, season_strength, imagine_0, imagine_1,
                    imagine_tier_0, imagine_tier_1, damage, dps, share_pct, crit_pct, lucky_pct,
                    hits, deaths
             FROM encounter_players WHERE encounter_id = ?1 ORDER BY slot",
        )?;
        record.players = stmt
            .query_map(params![id], |row| {
                Ok(PlayerRecord {
                    uid: row.get(0)?,
                    name: row.get(1)?,
                    class: row
                        .get::<_, Option<String>>(2)?
                        .as_deref()
                        .and_then(class_from_name),
                    ability_score: row
                        .get::<_, Option<i64>>(3)?
                        .map(|v| u32::try_from(v).unwrap_or(0)),
                    season_strength: row
                        .get::<_, Option<i64>>(4)?
                        .map(|v| u32::try_from(v).unwrap_or(0)),
                    imagines: [row.get(5)?, row.get(6)?],
                    imagine_tiers: [row.get(7)?, row.get(8)?],
                    damage: row.get(9)?,
                    dps: row.get(10)?,
                    share_pct: row.get::<_, f64>(11)? as f32,
                    crit_pct: row.get::<_, f64>(12)? as f32,
                    lucky_pct: row.get::<_, f64>(13)? as f32,
                    hits: u64::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
                    deaths: u32::try_from(row.get::<_, i64>(15)?).unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(record))
    }

    fn delete(&mut self, id: i64) -> Result<(), HistoryError> {
        self.conn
            .execute("DELETE FROM encounters WHERE id = ?1", params![id])?;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), HistoryError> {
        self.conn.execute("DELETE FROM encounters", params![])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsr_meter::Class;

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

    #[test]
    fn inserting_an_encounter_returns_its_id() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let record = sample_record(1_000, 10_000, vec![sample_player(1, "Alice")]);
        let id = store.insert(&record).unwrap();
        assert_eq!(id, Some(1));
    }

    #[test]
    fn a_fight_shorter_than_the_floor_is_not_inserted() {
        let policy = RetentionPolicy {
            min_duration_ms: 5_000,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        let record = sample_record(1_000, 1_000, vec![sample_player(1, "Alice")]);

        let id = store.insert(&record).unwrap();

        assert_eq!(id, None);
        assert!(store.list(10).unwrap().is_empty());
    }

    #[test]
    fn list_is_newest_first() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap();
        store
            .insert(&sample_record(2_000, 10_000, vec![sample_player(2, "Bob")]))
            .unwrap();

        let list = store.list(10).unwrap();

        assert_eq!(
            list.iter().map(|e| e.ended_at_ms).collect::<Vec<_>>(),
            vec![2_000, 1_000]
        );
    }

    #[test]
    fn list_honours_its_limit() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        for i in 0..5 {
            store
                .insert(&sample_record(
                    1_000 + i,
                    10_000,
                    vec![sample_player(1, "Alice")],
                ))
                .unwrap();
        }

        assert_eq!(store.list(2).unwrap().len(), 2);
    }

    #[test]
    fn load_returns_every_player_row_in_slot_order() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let id = store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice"), sample_player(2, "Bob")],
            ))
            .unwrap()
            .unwrap();

        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(
            loaded
                .players
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Alice".to_string(), "Bob".to_string()]
        );
    }

    #[test]
    fn load_round_trips_the_class_of_each_player() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let id = store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap()
            .unwrap();

        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(loaded.players[0].class, Some(Class::FrostMage));
    }

    #[test]
    fn load_of_a_missing_id_is_none() {
        let store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        assert_eq!(store.load(999).unwrap(), None);
    }

    #[test]
    fn the_count_cap_prunes_the_oldest() {
        let policy = RetentionPolicy {
            max_encounters: 3,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        for i in 0..5 {
            store
                .insert(&sample_record(
                    1_000 + i,
                    10_000,
                    vec![sample_player(1, "Alice")],
                ))
                .unwrap();
        }

        let list = store.list(10).unwrap();

        assert_eq!(
            list.iter().map(|e| e.ended_at_ms).collect::<Vec<_>>(),
            vec![1_004, 1_003, 1_002]
        );
    }

    #[test]
    fn the_age_cap_prunes_stale_encounters() {
        let policy = RetentionPolicy {
            max_age_days: 1,
            max_encounters: 0,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        let three_days_ms = 3 * 86_400_000;
        let t = 10 * 86_400_000_u64;

        store
            .insert(&sample_record(
                t - three_days_ms,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap();
        store
            .insert(&sample_record(t, 10_000, vec![sample_player(2, "Bob")]))
            .unwrap();

        assert_eq!(store.list(10).unwrap().len(), 1);
    }

    #[test]
    fn a_zero_count_cap_prunes_nothing() {
        let policy = RetentionPolicy {
            max_encounters: 0,
            max_age_days: 0,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        for i in 0..10 {
            store
                .insert(&sample_record(
                    1_000 + i,
                    10_000,
                    vec![sample_player(1, "Alice")],
                ))
                .unwrap();
        }

        assert_eq!(store.list(20).unwrap().len(), 10);
    }

    #[test]
    fn pruning_leaves_no_orphan_player_rows() {
        let policy = RetentionPolicy {
            max_encounters: 3,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        for i in 0..5 {
            store
                .insert(&sample_record(
                    1_000 + i,
                    10_000,
                    vec![sample_player(1, "Alice")],
                ))
                .unwrap();
        }

        let player_row_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_players", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(player_row_count, 3);
    }

    #[test]
    fn delete_removes_only_that_encounter() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let id1 = store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap()
            .unwrap();
        let id2 = store
            .insert(&sample_record(2_000, 10_000, vec![sample_player(2, "Bob")]))
            .unwrap()
            .unwrap();

        store.delete(id1).unwrap();

        assert_eq!(store.load(id1).unwrap(), None);
        assert!(store.load(id2).unwrap().is_some());
    }

    #[test]
    fn clear_empties_both_tables() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![sample_player(1, "Alice")],
            ))
            .unwrap();

        store.clear().unwrap();

        let encounter_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounters", [], |row| row.get(0))
            .unwrap();
        let player_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_players", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((encounter_count, player_count), (0, 0));
    }

    #[test]
    fn a_fresh_database_records_the_schema_version() {
        let store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn opening_twice_reuses_the_existing_schema() {
        let path =
            std::env::temp_dir().join(format!("bpsr-history-reopen-{}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);

        {
            let mut store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
            store
                .insert(&sample_record(
                    1_000,
                    10_000,
                    vec![sample_player(1, "Alice")],
                ))
                .unwrap();
        }

        let store2 = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let list = store2.list(10).unwrap();

        let _ = fs::remove_file(&path);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn an_unknown_schema_version_starts_a_fresh_file() {
        let path = std::env::temp_dir().join(format!(
            "bpsr-history-unknown-version-{}.sqlite",
            std::process::id()
        ));
        let bak_path = path.with_extension("v99.bak");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        {
            let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
            store.conn.pragma_update(None, "user_version", 99).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let list = store.list(10).unwrap();
        let bak_exists = bak_path.exists();

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        assert!(list.is_empty() && bak_exists);
    }
}
