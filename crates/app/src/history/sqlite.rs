//! `SqliteHistory`: the [`super::HistoryStore`] implementation this build
//! ships (issue #39). The **only** file the spec §10 JSONL fallback would
//! replace — everything else in `crates/app/src/history` talks to the trait,
//! not to SQL.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use bpsr_meter::{Class, EntityId, EntityKind};

use super::{
    EncounterRecord, EncounterSummary, HistoryError, HistoryStore, PlayerRecord, RetentionPolicy,
    SCHEMA_VERSION, SkillRecord,
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
    /// file is used as-is, a file stamped with a *known older* version is
    /// migrated forward in place by [`migrate`] (issue #222), and only a
    /// version this build has never heard of — a downgrade, or a hand-edited
    /// file — is renamed aside (`<path>.v<n>.bak`) and replaced with a new
    /// empty schema, since there is nothing to migrate from.
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
            v if v > 0 && v < SCHEMA_VERSION => {
                log::info!(
                    "history db: migrating {} from schema v{v} to v{SCHEMA_VERSION}",
                    path.display()
                );
                // SQLite DDL (including `ALTER TABLE`) is transactional, so
                // wrapping every migration step and the `user_version` bump
                // in one transaction makes the whole upgrade atomic: either
                // every step lands and the version bumps, or (on error, or a
                // crash mid-migration) nothing commits and the file is left
                // reading its original `v`, ready to retry on next open.
                let tx = conn.unchecked_transaction()?;
                migrate(&tx, v)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
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
            meter_version   TEXT    NOT NULL,
            local_uid       INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_encounters_ended_at ON encounters(ended_at_ms DESC);
        CREATE TABLE IF NOT EXISTS encounter_players (
            encounter_id    INTEGER NOT NULL REFERENCES encounters(id) ON DELETE CASCADE,
            slot            INTEGER NOT NULL,
            uid             INTEGER NOT NULL,
            entity          INTEGER,
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
    conn.execute_batch(SKILLS_DDL)?;
    Ok(())
}

/// The `encounter_player_skills` DDL (issue #222), shared by [`init_schema`]
/// and the v1 → v2 step of [`migrate`] so a freshly created file and a
/// migrated one can never end up with different tables.
///
/// Keyed by `(encounter_id, slot, skill_slot)` rather than by skill id:
/// `slot` joins back to `encounter_players`' own `slot`, and `skill_slot`
/// preserves the damage-descending order the meter produced, so a loaded
/// breakdown needs no re-sort. The cascade is off `encounters(id)` — the
/// same parent `encounter_players` cascades from — so deleting one encounter
/// (retention pruning included) takes its skill rows with it.
const SKILLS_DDL: &str = "CREATE TABLE IF NOT EXISTS encounter_player_skills (
        encounter_id    INTEGER NOT NULL REFERENCES encounters(id) ON DELETE CASCADE,
        slot            INTEGER NOT NULL,
        skill_slot      INTEGER NOT NULL,
        skill_id        INTEGER NOT NULL,
        damage          INTEGER NOT NULL,
        share_pct       REAL    NOT NULL,
        crit_pct        REAL    NOT NULL,
        max_crit        INTEGER NOT NULL,
        avg_crit        REAL    NOT NULL,
        avg_white       REAL    NOT NULL,
        avg             REAL    NOT NULL,
        hits            INTEGER NOT NULL,
        crit_hits       INTEGER NOT NULL,
        hits_per_min    REAL    NOT NULL,
        PRIMARY KEY (encounter_id, slot, skill_slot)
    );";

/// Upgrades a file stamped with the known older version `from` to
/// `SCHEMA_VERSION`, in place. Every step here is *additive* by construction:
/// no existing row is rewritten or dropped, so an interrupted upgrade leaves
/// a file the previous version could still read, and a user's history is
/// never wiped to gain a column.
fn migrate(conn: &Connection, from: i32) -> Result<(), HistoryError> {
    // Every step below is plain DDL (`CREATE TABLE`/`ALTER TABLE`), which
    // SQLite runs transactionally like any other statement — safe to issue
    // inside the caller's transaction alongside the `user_version` bump.
    // v1 → v2 (issue #222): per-skill totals. Nothing but a new table, so
    // encounters saved before it keep every field they had and simply have
    // no skill rows to hand back.
    if from < 2 {
        conn.execute_batch(SKILLS_DDL)?;
    }
    // v2 → v3 (issues #373, #379): the local player's uid at the moment a
    // fight ended, and the live `EntityId` behind each saved player row.
    // Both plain `ALTER TABLE ... ADD COLUMN`s — nullable, so every row
    // written before this step simply reads back `NULL` (see
    // `EncounterRecord::to_snapshot`/`PlayerRecord::to_row`'s doc comments)
    // rather than needing a backfill.
    if from < 3 {
        conn.execute_batch(
            "ALTER TABLE encounters ADD COLUMN local_uid INTEGER;
             ALTER TABLE encounter_players ADD COLUMN entity INTEGER;",
        )?;
    }
    // v3 → v4 (issue #392): a pre-v3 `encounter_players` row (no `entity`)
    // whose stored `uid` falls outside the range `EntityId::from_display_uid`
    // can reconstruct has no loadable identity — `load` already skips such
    // rows (belt-and-braces below) but `encounters.player_count` was never
    // updated to match, so the history list showed a count `load` could not
    // actually return. This is a one-time data cleanup, not new DDL: remove
    // those rows (and their skill rows), recompute `player_count` for every
    // encounter that lost a row, and drop any encounter left with zero
    // players so it also disappears from the list. `total_damage` and each
    // surviving row's `share_pct` are left untouched — they record the real
    // fight, not the reconstructable roster.
    if from < 4 {
        let doomed_players = conn.prepare(
            "SELECT COUNT(*) FROM encounter_players WHERE entity IS NULL AND (uid < ?1 OR uid > ?2)",
        )?.query_row(params![MIN_DISPLAY_UID, MAX_DISPLAY_UID], |row| row.get::<_, i64>(0))?;

        conn.execute(
            "DELETE FROM encounter_player_skills WHERE (encounter_id, slot) IN (
                SELECT encounter_id, slot FROM encounter_players
                WHERE entity IS NULL AND (uid < ?1 OR uid > ?2)
            )",
            params![MIN_DISPLAY_UID, MAX_DISPLAY_UID],
        )?;
        conn.execute(
            "DELETE FROM encounter_players WHERE entity IS NULL AND (uid < ?1 OR uid > ?2)",
            params![MIN_DISPLAY_UID, MAX_DISPLAY_UID],
        )?;
        conn.execute(
            "UPDATE encounters SET player_count = (
                SELECT COUNT(*) FROM encounter_players p WHERE p.encounter_id = encounters.id
            )",
            [],
        )?;
        let doomed_encounters = conn
            .prepare(
                "SELECT COUNT(*) FROM encounters WHERE id NOT IN (
                SELECT DISTINCT encounter_id FROM encounter_players
            )",
            )?
            .query_row([], |row| row.get::<_, i64>(0))?;
        conn.execute(
            "DELETE FROM encounter_player_skills WHERE encounter_id IN (
                SELECT id FROM encounters WHERE id NOT IN (
                    SELECT DISTINCT encounter_id FROM encounter_players
                )
            )",
            [],
        )?;
        conn.execute(
            "DELETE FROM encounters WHERE id NOT IN (
                SELECT DISTINCT encounter_id FROM encounter_players
            )",
            [],
        )?;
        if doomed_players > 0 || doomed_encounters > 0 {
            log::info!(
                "history db: v3 -> v4 migration removed {doomed_players} unreconstructable \
                 player row(s) and {doomed_encounters} now-empty encounter(s) (issue #392)"
            );
        }
    }
    Ok(())
}

/// The inclusive bounds `EntityId::from_display_uid` (`crates/meter/src/event.rs`)
/// accepts: a `uid` must fit in the 48-bit signed display-uid field it packs
/// into the reconstructed `EntityId`'s uuid, i.e. `-(2^47) ..= 2^47 - 1`.
/// Shared by the v3 → v4 migration above and the read-time fallback in
/// `load` so the two can never disagree about which rows are unreconstructable.
const MIN_DISPLAY_UID: i64 = -(1i64 << 47);
const MAX_DISPLAY_UID: i64 = (1i64 << 47) - 1;

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
                player_count, meter_version, local_uid
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                record.local_uid,
            ],
        )?;
        let encounter_id = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, entity, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            )?;
            let mut skill_stmt = tx.prepare(
                "INSERT INTO encounter_player_skills (
                    encounter_id, slot, skill_slot, skill_id, damage, share_pct, crit_pct,
                    max_crit, avg_crit, avg_white, avg, hits, crit_hits, hits_per_min
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for (slot, player) in record.players.iter().enumerate() {
                let slot_id = i64::try_from(slot).unwrap_or(i64::MAX);
                stmt.execute(params![
                    encounter_id,
                    slot_id,
                    player.uid,
                    player.entity,
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

                for (skill_slot, skill) in player.skills.iter().enumerate() {
                    skill_stmt.execute(params![
                        encounter_id,
                        slot_id,
                        i64::try_from(skill_slot).unwrap_or(i64::MAX),
                        skill.skill_id,
                        skill.damage,
                        f64::from(skill.share_pct),
                        f64::from(skill.crit_pct),
                        skill.max_crit,
                        skill.avg_crit,
                        skill.avg_white,
                        skill.avg,
                        i64::try_from(skill.hits).unwrap_or(i64::MAX),
                        i64::try_from(skill.crit_hits).unwrap_or(i64::MAX),
                        skill.hits_per_min,
                    ])?;
                }
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
                        boss_name, is_boss, scene_id, scene_name, title, subtitle, meter_version,
                        local_uid
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
                        local_uid: row.get(12)?,
                        players: Vec::new(),
                    })
                },
            )
            .optional()?;

        let Some(mut record) = base else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT uid, entity, name, class, ability_score, season_strength, imagine_0,
                    imagine_1, imagine_tier_0, imagine_tier_1, damage, dps, share_pct, crit_pct,
                    lucky_pct, hits, deaths, slot
             FROM encounter_players WHERE encounter_id = ?1 ORDER BY slot",
        )?;
        let loaded = stmt
            .query_map(params![id], |row| {
                let uid: i64 = row.get(0)?;
                let slot: i64 = row.get(17)?;
                // Issue #379: a pre-v3 row has no stored `entity` and reads
                // back `NULL` here; reconstruct the same `EntityId` a live
                // encounter would have derived for a bare display uid, so
                // `PlayerRecord::to_row` always has a real value to hand
                // back rather than needing its own `Option`.
                let entity = match row.get::<_, Option<i64>>(1)? {
                    Some(entity) => entity,
                    // Issue #392: a stored uid outside the 48-bit display-uid
                    // field has no reconstructable identity, and filing it
                    // under `EntityId::UNKNOWN` would merge every such row in
                    // the database into one player. The v3 → v4 migration
                    // (`migrate`, above) normally removes such rows — and
                    // recomputes `encounters.player_count` to match — the
                    // first time an older file is opened, so this is
                    // belt-and-braces for a row that somehow still slips
                    // through: drop it instead.
                    None => match EntityId::from_display_uid(uid, EntityKind::Player) {
                        Some(entity) => entity.0 as i64,
                        None => {
                            log::warn!(
                                "history: skipping pre-v3 row with out-of-range uid={uid} (issue #392)"
                            );
                            return Ok(None);
                        }
                    },
                };
                Ok(Some((slot, PlayerRecord {
                    uid,
                    entity,
                    name: row.get(2)?,
                    class: row
                        .get::<_, Option<String>>(3)?
                        .as_deref()
                        .and_then(class_from_name),
                    ability_score: row
                        .get::<_, Option<i64>>(4)?
                        .map(|v| u32::try_from(v).unwrap_or(0)),
                    season_strength: row
                        .get::<_, Option<i64>>(5)?
                        .map(|v| u32::try_from(v).unwrap_or(0)),
                    imagines: [row.get(6)?, row.get(7)?],
                    imagine_tiers: [row.get(8)?, row.get(9)?],
                    damage: row.get(10)?,
                    dps: row.get(11)?,
                    share_pct: row.get::<_, f64>(12)? as f32,
                    crit_pct: row.get::<_, f64>(13)? as f32,
                    lucky_pct: row.get::<_, f64>(14)? as f32,
                    hits: u64::try_from(row.get::<_, i64>(15)?).unwrap_or(0),
                    deaths: u32::try_from(row.get::<_, i64>(16)?).unwrap_or(0),
                    skills: Vec::new(),
                })))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        // A skipped row leaves a hole in the `slot` sequence, so the skill
        // fan-out below can no longer use a player's index as its slot.
        let mut slot_to_index: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        for (slot, player) in loaded.into_iter().flatten() {
            slot_to_index.insert(slot, record.players.len());
            record.players.push(player);
        }

        // Belt-and-braces: the v3 → v4 migration normally drops an encounter
        // that loses every player row before this point is ever reached, but
        // an empty roster here (e.g. from a row that slipped past the
        // migration) must still not be handed back as a loadable encounter.
        if record.players.is_empty() {
            log::warn!("history: encounter {id} has no loadable players (issue #392)");
            return Ok(None);
        }

        // Issue #222: one query for the whole encounter's breakdown, fanned out by
        // slot through slot_to_index (a skipped #392 row leaves a hole, so a
        // player's index is not its slot). An encounter written before schema
        // v2 matches no rows here and keeps every player's breakdown empty.
        let mut stmt = self.conn.prepare(
            "SELECT slot, skill_id, damage, share_pct, crit_pct, max_crit, avg_crit,
                    avg_white, avg, hits, crit_hits, hits_per_min
             FROM encounter_player_skills WHERE encounter_id = ?1 ORDER BY slot, skill_slot",
        )?;
        let skills = stmt.query_map(params![id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                SkillRecord {
                    skill_id: row.get(1)?,
                    damage: row.get(2)?,
                    share_pct: row.get::<_, f64>(3)? as f32,
                    crit_pct: row.get::<_, f64>(4)? as f32,
                    max_crit: row.get(5)?,
                    avg_crit: row.get(6)?,
                    avg_white: row.get(7)?,
                    avg: row.get(8)?,
                    hits: u64::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
                    crit_hits: u64::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
                    hits_per_min: row.get(11)?,
                },
            ))
        })?;
        for entry in skills {
            let (slot, skill) = entry?;
            if let Some(player) = slot_to_index
                .get(&slot)
                .and_then(|index| record.players.get_mut(*index))
            {
                player.skills.push(skill);
            }
        }

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

    fn sample_player(uid: i64, name: &str) -> PlayerRecord {
        PlayerRecord {
            uid,
            entity: EntityId::from_display_uid(uid, EntityKind::Player)
                .expect("in-range test uid")
                .0 as i64,
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
            local_uid: None,
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
    fn pruning_leaves_no_orphan_player_or_skill_rows() {
        let policy = RetentionPolicy {
            max_encounters: 3,
            ..RetentionPolicy::default()
        };
        let mut store = SqliteHistory::in_memory(policy).unwrap();
        for i in 0..5 {
            let mut alice = sample_player(1, "Alice");
            alice.skills = vec![sample_skill(101, 3_000)];
            store
                .insert(&sample_record(1_000 + i, 10_000, vec![alice]))
                .unwrap();
        }

        let player_row_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_players", [], |row| {
                row.get(0)
            })
            .unwrap();
        let skill_row_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_player_skills", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(player_row_count, 3);
        assert_eq!(
            skill_row_count, 3,
            "each surviving encounter's single skill row must survive the prune"
        );
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

    fn sample_skill(skill_id: i32, damage: i64) -> SkillRecord {
        SkillRecord {
            skill_id,
            damage,
            share_pct: 60.5,
            crit_pct: 25.25,
            max_crit: damage / 2,
            avg_crit: 1_500.5,
            avg_white: 900.25,
            avg: 1_200.75,
            hits: 8,
            crit_hits: 2,
            hits_per_min: 40.5,
        }
    }

    /// The v1 DDL verbatim (the schema that shipped before issue #222), so
    /// the migration is exercised against what is actually on disk in the
    /// wild rather than against today's `init_schema`.
    const V1_SCHEMA: &str = "CREATE TABLE encounters (
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
        CREATE INDEX idx_encounters_ended_at ON encounters(ended_at_ms DESC);
        CREATE TABLE encounter_players (
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
        );";

    #[test]
    fn loading_an_encounter_restores_each_player_s_skill_rows() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let mut alice = sample_player(1, "Alice");
        alice.skills = vec![sample_skill(101, 3_000), sample_skill(102, 2_000)];
        let id = store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![alice.clone(), sample_player(2, "Bob")],
            ))
            .unwrap()
            .unwrap();

        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(loaded.players[0].skills, alice.skills);
        assert!(
            loaded.players[1].skills.is_empty(),
            "a player who never hit anything keeps an empty breakdown"
        );
    }

    #[test]
    fn deleting_an_encounter_drops_its_skill_rows() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let mut alice = sample_player(1, "Alice");
        alice.skills = vec![sample_skill(101, 3_000)];
        let id = store
            .insert(&sample_record(1_000, 10_000, vec![alice]))
            .unwrap()
            .unwrap();

        store.delete(id).unwrap();

        let skill_rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_player_skills", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(skill_rows, 0);
    }

    /// Issue #222: an existing history file must open, keep every encounter
    /// it already holds, and start recording skill rows from then on — the
    /// pre-#222 encounters simply have none.
    #[test]
    fn a_v1_database_migrates_in_place_and_keeps_its_encounters() {
        let path = crate::history::temp_history_path("v1-migration");
        let bak_path = path.with_extension("v1.bak");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 1, '0.2.2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                 ) VALUES (1, 0, 1, 'Alice', 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                           5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let mut store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let old = store.load(1).unwrap().unwrap();
        let mut bob = sample_player(2, "Bob");
        bob.skills = vec![sample_skill(101, 3_000)];
        let new_id = store
            .insert(&sample_record(2_000, 10_000, vec![bob.clone()]))
            .unwrap()
            .unwrap();
        let fresh = store.load(new_id).unwrap().unwrap();
        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        drop(store);
        let bak_exists = bak_path.exists();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        assert_eq!(old.players.len(), 1, "the pre-#222 encounter survives");
        assert_eq!(old.players[0].name, "Alice");
        assert!(
            old.players[0].skills.is_empty(),
            "an encounter saved before the skill table simply has no skill rows"
        );
        assert_eq!(fresh.players[0].skills, bob.skills);
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            !bak_exists,
            "a migratable file is upgraded in place, never renamed aside"
        );
    }

    /// Issue #373: `EncounterRecord::local_uid` round-trips through a real
    /// insert/load cycle, not just through the in-memory `to_snapshot`
    /// conversion.
    #[test]
    fn inserting_and_loading_round_trips_local_uid() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let mut record = sample_record(1_000, 10_000, vec![sample_player(1, "Alice")]);
        record.local_uid = Some(1);
        let id = store.insert(&record).unwrap().unwrap();

        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(loaded.local_uid, Some(1));
    }

    /// Issue #379: two players who share a recycled display `uid` in the
    /// same live encounter must stay two distinct rows on reload, each
    /// carrying its own `entity`.
    #[test]
    fn two_players_sharing_a_recycled_uid_stay_distinct_by_entity() {
        let mut store = SqliteHistory::in_memory(RetentionPolicy::default()).unwrap();
        let mut first = sample_player(1, "Alice");
        first.entity = EntityId::from_display_uid(1, EntityKind::Player)
            .expect("in-range test uid")
            .0 as i64;
        let mut second = sample_player(1, "Bob");
        second.entity = first.entity | 0x1;
        let id = store
            .insert(&sample_record(
                1_000,
                10_000,
                vec![first.clone(), second.clone()],
            ))
            .unwrap()
            .unwrap();

        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(loaded.players[0].uid, 1);
        assert_eq!(loaded.players[1].uid, 1);
        assert_eq!(loaded.players[0].entity, first.entity);
        assert_eq!(loaded.players[1].entity, second.entity);
        assert_ne!(loaded.players[0].entity, loaded.players[1].entity);

        let snapshot = loaded.to_snapshot();
        assert_ne!(snapshot.rows[0].entity, snapshot.rows[1].entity);
    }

    /// Issue #373/#379: a v2 database has neither `encounters.local_uid` nor
    /// `encounter_players.entity`. Migrating it forward must add both
    /// columns and leave every existing row reading back `local_uid: None`
    /// and an `entity` reconstructed from its bare `uid`, exactly like a
    /// pre-v3 build would have derived it live.
    #[test]
    fn a_v2_database_migrates_in_place_and_backfills_entity() {
        let path = crate::history::temp_history_path("v2-migration");
        let bak_path = path.with_extension("v2.bak");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(SKILLS_DDL).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 1, '0.2.2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                 ) VALUES (1, 0, 1, 'Alice', 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                           5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let loaded = store.load(1).unwrap().unwrap();
        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        drop(store);
        let bak_exists = bak_path.exists();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak_path);

        assert_eq!(loaded.local_uid, None);
        assert_eq!(
            loaded.players[0].entity,
            EntityId::from_display_uid(1, EntityKind::Player)
                .expect("in-range test uid")
                .0 as i64
        );
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            !bak_exists,
            "a migratable file is upgraded in place, never renamed aside"
        );
    }

    /// Issue #392: a pre-v3 row whose stored `uid` falls outside the 48-bit
    /// display-uid field has no reconstructable `EntityId`, and filing it
    /// under `EntityId::UNKNOWN` would merge every such row into one player.
    /// Skip those rows instead and keep the loadable ones.
    #[test]
    fn a_v2_row_with_an_out_of_range_uid_is_skipped() {
        let path = crate::history::temp_history_path("v2-out-of-range-uid");
        let _ = fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(SKILLS_DDL).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 2, '0.2.2')",
                [],
            )
            .unwrap();
            for (slot, uid, name) in [(0i64, (1i64 << 47) + 1, "Garbage"), (1, 5, "Alice")] {
                conn.execute(
                    "INSERT INTO encounter_players (
                        encounter_id, slot, uid, name, class, ability_score, season_strength,
                        imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                        damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                     ) VALUES (1, ?1, ?2, ?3, 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                               5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                    params![slot, uid, name],
                )
                .unwrap();
            }
            for (slot, skill_id) in [(0i64, 111i64), (1, 222)] {
                conn.execute(
                    "INSERT INTO encounter_player_skills (
                        encounter_id, slot, skill_slot, skill_id, damage, share_pct, crit_pct,
                        max_crit, avg_crit, avg_white, avg, hits, crit_hits, hits_per_min
                     ) VALUES (1, ?1, 0, ?2, 1000, 100.0, 0.0, 0, 0.0, 0.0, 0.0, 1, 0, 0.0)",
                    params![slot, skill_id],
                )
                .unwrap();
            }
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let loaded = store.load(1).unwrap().unwrap();
        drop(store);
        let _ = fs::remove_file(&path);

        assert_eq!(
            loaded.players.len(),
            1,
            "the out-of-range row is skipped, not loaded as UNKNOWN"
        );
        assert_eq!(loaded.players[0].uid, 5);
        assert_eq!(
            loaded.players[0].entity,
            EntityId::from_display_uid(5, EntityKind::Player)
                .expect("in-range test uid")
                .0 as i64
        );
        assert_eq!(
            loaded.players[0].skills.len(),
            1,
            "the surviving player's skills must come from its own slot, not its index"
        );
        assert_eq!(loaded.players[0].skills[0].skill_id, 222);
    }

    /// Issue #392: the v3 → v4 migration itself (not just the read-time
    /// skip) must remove an unreconstructable pre-v3 row and its skills, and
    /// recompute `encounters.player_count` so the history list agrees with
    /// what `load` actually returns.
    #[test]
    fn migrating_a_v2_database_removes_out_of_range_rows_and_recomputes_player_count() {
        let path = crate::history::temp_history_path("v2-migration-drops-out-of-range-uid");
        let _ = fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(SKILLS_DDL).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 2, '0.2.2')",
                [],
            )
            .unwrap();
            for (slot, uid, name) in [(0i64, (1i64 << 47) + 1, "Garbage"), (1, 5, "Alice")] {
                conn.execute(
                    "INSERT INTO encounter_players (
                        encounter_id, slot, uid, name, class, ability_score, season_strength,
                        imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                        damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                     ) VALUES (1, ?1, ?2, ?3, 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                               5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                    params![slot, uid, name],
                )
                .unwrap();
            }
            for (slot, skill_id) in [(0i64, 111i64), (1, 222)] {
                conn.execute(
                    "INSERT INTO encounter_player_skills (
                        encounter_id, slot, skill_slot, skill_id, damage, share_pct, crit_pct,
                        max_crit, avg_crit, avg_white, avg, hits, crit_hits, hits_per_min
                     ) VALUES (1, ?1, 0, ?2, 1000, 100.0, 0.0, 0, 0.0, 0.0, 0.0, 1, 0, 0.0)",
                    params![slot, skill_id],
                )
                .unwrap();
            }
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();

        let remaining_players: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM encounter_players WHERE uid = ?1",
                params![(1i64 << 47) + 1],
                |row| row.get(0),
            )
            .unwrap();
        let remaining_skills: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM encounter_player_skills WHERE slot = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let stored_player_count: i64 = store
            .conn
            .query_row(
                "SELECT player_count FROM encounters WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let list = store.list(10).unwrap();
        let loaded = store.load(1).unwrap().unwrap();
        drop(store);
        let _ = fs::remove_file(&path);

        assert_eq!(remaining_players, 0, "the unreconstructable row is deleted");
        assert_eq!(remaining_skills, 0, "its skill rows are deleted with it");
        assert_eq!(stored_player_count, 1, "player_count is recomputed");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].player_count, 1);
        assert_eq!(loaded.players.len(), 1);
        assert_eq!(loaded.players[0].uid, 5);
        assert_eq!(loaded.players[0].skills.len(), 1);
        assert_eq!(loaded.players[0].skills[0].skill_id, 222);
    }

    /// Issue #392: if the migration removes every player row an encounter
    /// had, the encounter itself (and any skill rows still pointing at it)
    /// must be removed too, so it also disappears from `list`.
    #[test]
    fn migrating_a_v2_database_drops_an_encounter_left_with_no_players() {
        let path = crate::history::temp_history_path("v2-migration-drops-empty-encounter");
        let _ = fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(SKILLS_DDL).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 1, '0.2.2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                 ) VALUES (1, 0, ?1, 'Garbage', 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                           5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                params![(1i64 << 47) + 1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encounter_player_skills (
                    encounter_id, slot, skill_slot, skill_id, damage, share_pct, crit_pct,
                    max_crit, avg_crit, avg_white, avg, hits, crit_hits, hits_per_min
                 ) VALUES (1, 0, 0, 111, 1000, 100.0, 0.0, 0, 0.0, 0.0, 0.0, 1, 0, 0.0)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let list = store.list(10).unwrap();
        let encounter_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounters", [], |row| row.get(0))
            .unwrap();
        let skill_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM encounter_player_skills", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(store);
        let _ = fs::remove_file(&path);

        assert!(
            list.is_empty(),
            "the emptied encounter is gone from the list"
        );
        assert_eq!(encounter_count, 0);
        assert_eq!(skill_count, 0, "its orphaned skill rows are gone too");
    }

    /// Issue #392: if every row in an encounter is skipped as unloadable,
    /// `load` must not hand back an encounter with an empty roster.
    #[test]
    fn a_v2_row_that_is_entirely_out_of_range_yields_no_encounter() {
        let path = crate::history::temp_history_path("v2-out-of-range-uid-only");
        let _ = fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
            conn.execute_batch(SKILLS_DDL).unwrap();
            conn.execute(
                "INSERT INTO encounters (
                    ended_at_ms, duration_ms, total_damage, total_dps, boss_monster_id,
                    boss_name, is_boss, scene_id, scene_name, title, subtitle,
                    player_count, meter_version
                 ) VALUES (1000, 10000, 10000, 1000.0, 7, 'Boss', 1, 3, 'Scene', 'Boss',
                           'Scene', 1, '0.2.2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encounter_players (
                    encounter_id, slot, uid, name, class, ability_score, season_strength,
                    imagine_0, imagine_1, imagine_tier_0, imagine_tier_1,
                    damage, dps, share_pct, crit_pct, lucky_pct, hits, deaths
                 ) VALUES (1, 0, ?1, 'Garbage', 'FrostMage', 999, 42, 1, NULL, 3, NULL,
                           5000, 500.0, 33.3, 12.5, 6.25, 40, 2)",
                params![(1i64 << 47) + 1],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        let loaded = store.load(1).unwrap();
        drop(store);
        let _ = fs::remove_file(&path);

        assert!(
            loaded.is_none(),
            "an encounter with no loadable players must not be returned"
        );
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
