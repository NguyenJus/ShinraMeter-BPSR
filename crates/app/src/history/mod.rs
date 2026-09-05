//! Durable encounter history (issue #39): the storage core that turns a
//! just-ended fight into a row a user can browse minutes or days later.
//!
//! This module defines owned DTOs (`EncounterRecord`/`PlayerRecord`/
//! `EncounterSummary`) rather than putting `serde`/SQL bindings on
//! `bpsr_meter`'s live types directly (spec DECISION D1): `PlayerRow`'s and
//! `EncounterInfo`'s `&'static str` name fields borrow from generated
//! `tables.rs` and cannot round-trip through a database, and the hot-path
//! meter types must not grow a persistence contract for a feature living two
//! crates away. [`record_from_snapshot`] and [`EncounterRecord::to_snapshot`]
//! are the two directions of that boundary.
//!
//! The actual backend lives behind the [`HistoryStore`] trait
//! ([`sqlite::SqliteHistory`] today) so a future swap — e.g. the spec §10
//! JSONL fallback — replaces one file and nothing else in this module or its
//! callers.

pub mod sanitize;
pub mod sqlite;
pub mod writer;

use std::path::PathBuf;

use bpsr_meter::{Class, EncounterInfo, EntityId, EntityKind, PlayerRow, SkillRow, Snapshot};

/// Where the encounter-history database (issue #39) lives:
/// `%APPDATA%\ShinraMeter-BPSR\history.sqlite`. `SHINRA_HISTORY_DB` overrides
/// it outright. Lives here — rather than staying a `main.rs`-local helper —
/// so both `main`'s `HistoryHandle::spawn` and the lib-side session-bundle
/// export (`crate::bundle`, driven from `crate::ui`) name the exact same
/// file without the lib crate depending back on the bin crate for it.
/// Mirrors `settings::settings_path`/`inspect::dump_path`/
/// `logging::log_file_path`'s own `paths::resolve` calls.
pub fn history_db_path() -> PathBuf {
    let (path, warning) = crate::paths::resolve(
        std::env::var("SHINRA_HISTORY_DB").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
        &["ShinraMeter-BPSR", "history.sqlite"],
        "ShinraMeter-BPSR-history.sqlite",
        "APPDATA is not set; falling back to a working-directory file for the encounter history",
    );
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }
    path
}

/// Schema version this build writes and reads (spec §5.4). Bumped whenever
/// the DDL in `sqlite::init_schema` changes.
///
/// v1 → v2 (issue #222) added `encounter_player_skills`. A file stamped with
/// an *older* known version is migrated forward in place by
/// `sqlite::migrate`, so an existing history survives the upgrade with its
/// pre-v2 encounters simply carrying no skill rows. Only a version this build
/// has never heard of (a downgrade, or a hand-edited file) is still renamed
/// aside and replaced, since there is nothing to migrate *from*.
pub const SCHEMA_VERSION: i32 = 2;

/// Retention rules, applied inside every `HistoryStore::insert` (spec §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep at most this many encounters; `0` disables the count prune.
    pub max_encounters: u32,
    /// Drop encounters older than this many days; `0` disables the age prune.
    pub max_age_days: u32,
    /// Never record a fight shorter than this.
    pub min_duration_ms: u64,
}

impl Default for RetentionPolicy {
    /// Spec §5.5's defaults: 500 encounters, 90 days, a 5s floor.
    fn default() -> Self {
        Self {
            max_encounters: 500,
            max_age_days: 90,
            min_duration_ms: 5_000,
        }
    }
}

/// One saved row of a player's skill breakdown (issue #222) — the owned
/// twin of `bpsr_meter::SkillRow`, kept as its own type for the same reason
/// [`PlayerRecord`] is: the meter's hot-path types must not grow a
/// persistence contract. Field for field identical to `SkillRow`, whose
/// fields are all already owned, so the two conversions below are plain
/// copies.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRecord {
    pub skill_id: i32,
    pub damage: i64,
    /// Share of this *player's* damage, not the encounter's — same meaning
    /// as `SkillRow::share_pct`, stored as-computed rather than re-derived
    /// on load so a historical breakdown can never disagree with what the
    /// live window showed.
    pub share_pct: f32,
    pub crit_pct: f32,
    pub max_crit: i64,
    pub avg_crit: f64,
    pub avg_white: f64,
    pub avg: f64,
    pub hits: u64,
    pub crit_hits: u64,
    pub hits_per_min: f64,
}

impl From<&SkillRow> for SkillRecord {
    fn from(row: &SkillRow) -> Self {
        Self {
            skill_id: row.skill_id,
            damage: row.damage,
            share_pct: row.share_pct,
            crit_pct: row.crit_pct,
            max_crit: row.max_crit,
            avg_crit: row.avg_crit,
            avg_white: row.avg_white,
            avg: row.avg,
            hits: row.hits,
            crit_hits: row.crit_hits,
            hits_per_min: row.hits_per_min,
        }
    }
}

impl SkillRecord {
    /// The other direction, used by [`PlayerRecord::to_row`] so a historical
    /// breakdown window draws through the exact same `SkillRow` path a live
    /// one does.
    fn to_skill_row(&self) -> SkillRow {
        SkillRow {
            skill_id: self.skill_id,
            damage: self.damage,
            share_pct: self.share_pct,
            crit_pct: self.crit_pct,
            max_crit: self.max_crit,
            avg_crit: self.avg_crit,
            avg_white: self.avg_white,
            avg: self.avg,
            hits: self.hits,
            crit_hits: self.crit_hits,
            hits_per_min: self.hits_per_min,
            // Issue #338's absorbed/immune channels predate this on-disk
            // record shape; a historical row has no way to have recorded
            // them, so a replayed `SkillRow` always reads `0` here rather
            // than growing the sqlite schema for a field no saved encounter
            // can supply.
            absorbed_total: 0,
            immune_total: 0,
        }
    }
}

/// One saved player row (spec §5.3's `encounter_players`). The owned twin of
/// `bpsr_meter::PlayerRow` — same fields, `String` in place of the meter's
/// already-owned `name` (which is itself a `String`, so this is a plain
/// field-for-field mirror kept as its own type so the meter crate never has
/// to know this feature exists).
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerRecord {
    pub uid: i64,
    pub name: String,
    pub class: Option<Class>,
    pub ability_score: Option<u32>,
    pub season_strength: Option<u32>,
    pub imagines: [Option<i32>; 2],
    pub imagine_tiers: [Option<i32>; 2],
    pub damage: i64,
    pub dps: f64,
    pub share_pct: f32,
    pub crit_pct: f32,
    pub lucky_pct: f32,
    pub hits: u64,
    pub deaths: u32,
    /// This player's per-skill breakdown (issue #222), damage-descending —
    /// the order `SkillRow` arrives in and the order the breakdown window
    /// draws, preserved verbatim through the `slot` column rather than
    /// re-sorted on load.
    pub skills: Vec<SkillRecord>,
}

impl From<&PlayerRow> for PlayerRecord {
    fn from(row: &PlayerRow) -> Self {
        Self {
            uid: row.uid,
            name: row.name.clone(),
            class: row.class,
            ability_score: row.ability_score,
            season_strength: row.season_strength,
            imagines: row.imagines,
            imagine_tiers: row.imagine_tiers,
            damage: row.damage,
            dps: row.dps,
            share_pct: row.share_pct,
            crit_pct: row.crit_pct,
            lucky_pct: row.lucky_pct,
            hits: row.hits,
            deaths: row.deaths,
            skills: row.skills.iter().map(SkillRecord::from).collect(),
        }
    }
}

impl PlayerRecord {
    /// The other direction of the `From<&PlayerRow>` impl above, used by
    /// [`EncounterRecord::to_snapshot`] to rebuild a live-shaped row from a
    /// saved one.
    fn to_row(&self) -> PlayerRow {
        PlayerRow {
            uid: self.uid,
            // Issue #335: history predates full `EntityId`s, so the schema
            // has no stored entity to replay — reconstruct the canonical
            // one a display uid/kind would have with both flag bits clear
            // (`EntityId::from_display_uid`), the same id a live player
            // with no summon/mirror flags gets. Two saved rows can share a
            // display uid (`uid_recycle_separates_entities`'s golden), but
            // never this reconstruction plus its kind, since history only
            // ever persists players.
            entity: EntityId::from_display_uid(self.uid, EntityKind::Player).0 as i64,
            name: self.name.clone(),
            class: self.class,
            ability_score: self.ability_score,
            season_strength: self.season_strength,
            imagines: self.imagines,
            imagine_tiers: self.imagine_tiers,
            damage: self.damage,
            dps: self.dps,
            share_pct: self.share_pct,
            crit_pct: self.crit_pct,
            lucky_pct: self.lucky_pct,
            hits: self.hits,
            deaths: self.deaths,
            // Issue #254: the schema has no death-time column, so a
            // replayed row reports the total as unmeasured rather than as
            // zero. See `PlayerRow::dead_ms`.
            dead_ms: None,
            // Issue #222: persisted since schema v2, so a historical row
            // opens the same breakdown a live one does. Encounters saved
            // before v2 have no skill rows and land here empty.
            skills: self.skills.iter().map(SkillRecord::to_skill_row).collect(),
            // Issue #245: the Heal / Skill dealt / Skill received
            // breakdowns are live-only. The saved-fight schema persists
            // one per-skill list — the damage one — and widening it would
            // be a fourth schema revision for data the window already has
            // an honest empty state for ("No per-skill data recorded for
            // this fight", `skill_window_empty_message`). Left as a
            // follow-up rather than smuggled into this change.
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            // Issue #267: same story as `heals`/`dealt`/`received`/`casts`
            // above — the Buff tab is live-only, and the schema has no
            // per-buff column to replay.
            buffs: Vec::new(),
            // Issue #338: same story as `dead_ms` above — no schema column,
            // so a replayed row reads unmeasured/absent rather than a real
            // (and misleadingly precise) zero-or-unknown value.
            absorbed_total: 0,
            immune_total: 0,
            shield: None,
        }
    }
}

/// One saved encounter, players included (spec §5.3's `encounters`).
#[derive(Debug, Clone, PartialEq)]
pub struct EncounterRecord {
    pub ended_at_ms: u64,
    pub duration_ms: u64,
    pub total_damage: i64,
    pub total_dps: f64,
    pub boss_monster_id: Option<u32>,
    pub boss_name: Option<String>,
    pub is_boss: bool,
    pub scene_id: Option<u32>,
    pub scene_name: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub meter_version: String,
    pub players: Vec<PlayerRecord>,
}

/// One row of the history list — everything the list UI paints, and nothing
/// more, so browsing never loads every player row of every fight.
#[derive(Debug, Clone, PartialEq)]
pub struct EncounterSummary {
    pub id: i64,
    pub ended_at_ms: u64,
    pub duration_ms: u64,
    pub total_damage: i64,
    pub total_dps: f64,
    pub title: String,
    pub subtitle: Option<String>,
    pub player_count: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to create the history directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to move the unreadable history file aside: {0}")]
    RenameAside(std::io::Error),
    #[error("failed to copy the history database for sanitizing: {0}")]
    Copy(std::io::Error),
}

/// The narrow seam the storage backend lives behind (spec §10): swapping
/// SQLite for newline-delimited JSON replaces exactly one implementor of this
/// trait and nothing else.
pub trait HistoryStore: Send {
    /// Persists `record` and applies retention. Returns the new row id, or
    /// `Ok(None)` when the record was rejected by the duration floor.
    fn insert(&mut self, record: &EncounterRecord) -> Result<Option<i64>, HistoryError>;
    /// Newest first, at most `limit` rows.
    fn list(&self, limit: u32) -> Result<Vec<EncounterSummary>, HistoryError>;
    fn load(&self, id: i64) -> Result<Option<EncounterRecord>, HistoryError>;
    fn delete(&mut self, id: i64) -> Result<(), HistoryError>;
    fn clear(&mut self) -> Result<(), HistoryError>;
}

/// Builds the record for a just-ended fight (spec §5.7). `title`/`subtitle`
/// are supplied by the caller from `ui::encounter_title`/`encounter_subtitle`
/// (DECISION D2) — this module deliberately does not re-derive the naming
/// rules, so a historical fight's label can never drift when the naming rule
/// changes later. Returns `None` when the fight is not worth recording (D12:
/// no rows, or no damage). The duration floor is *not* checked here — that is
/// `HistoryStore::insert`'s job, since it is a `RetentionPolicy` concern, not
/// a "did this fight happen" one.
pub fn record_from_snapshot(
    snapshot: &Snapshot,
    ended_at_ms: u64,
    title: String,
    subtitle: Option<String>,
) -> Option<EncounterRecord> {
    if snapshot.rows.is_empty() || snapshot.total_damage <= 0 {
        return None;
    }

    Some(EncounterRecord {
        ended_at_ms,
        duration_ms: snapshot.duration_ms,
        total_damage: snapshot.total_damage,
        total_dps: snapshot.total_dps,
        boss_monster_id: snapshot.encounter.boss_monster_id,
        boss_name: snapshot.encounter.boss_name.map(str::to_string),
        is_boss: snapshot.encounter.is_boss,
        scene_id: snapshot.encounter.scene_id,
        scene_name: snapshot.encounter.scene_name.map(str::to_string),
        title,
        subtitle,
        meter_version: env!("CARGO_PKG_VERSION").to_string(),
        players: snapshot.rows.iter().map(PlayerRecord::from).collect(),
    })
}

impl EncounterRecord {
    /// Rebuilds a `Snapshot` so a saved fight renders through the *live*
    /// table path, `ui::draw_rows`/`draw_row` (DECISION D8). The
    /// `&'static str` name fields of `EncounterInfo` stay `None` — those are
    /// borrowed from generated tables and cannot come back from a database;
    /// the header gets its text from `title`/`subtitle` instead (DECISION
    /// D7, wired up in WP3).
    pub fn to_snapshot(&self) -> Snapshot {
        let rows: Vec<PlayerRow> = self.players.iter().map(PlayerRecord::to_row).collect();
        // Issue #338: `PlayerRecord::to_row` always zeroes a replayed row's
        // absorbed/immune totals (no schema column to replay them from —
        // see that function's doc comment), so these sum to `0` too; kept
        // as a real sum rather than a hardcoded `0` so this stays correct
        // the day a schema revision does persist them.
        let total_absorbed: i64 = rows.iter().map(|r| r.absorbed_total).sum();
        let total_immune: i64 = rows.iter().map(|r| r.immune_total).sum();
        Snapshot {
            duration_ms: self.duration_ms,
            total_damage: self.total_damage,
            total_dps: self.total_dps,
            total_absorbed,
            total_immune,
            rows,
            encounter: EncounterInfo {
                boss_monster_id: self.boss_monster_id,
                is_boss: self.is_boss,
                scene_id: self.scene_id,
                ..EncounterInfo::default()
            },
            // A rebuilt-from-history snapshot has no live capture thread to
            // ask; it renders through the same live table path regardless,
            // and is never checked for this (see `Snapshot::capture_alive`).
            // Separately, the local player's uid is not persisted in
            // history.sqlite at all — the current schema (SCHEMA_VERSION =
            // 2) has no column for it, so a rebuilt snapshot can never
            // identify "you" even if it wanted to. Persisting it needs a
            // schema bump plus a sanitizer remap (#353); tracked as a
            // follow-up in #373.
            local_uid: None,
            capture_alive: true,
        }
    }
}

/// `unix_ms` rendered as local `YYYY-MM-DD HH:MM` (DECISION D10). Returns an
/// em dash for the (practically unreachable) case where the millisecond
/// value doesn't map to a single local instant.
pub fn format_local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(unix_ms as i64) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "—".to_string(),
    }
}

/// `duration_ms` rendered as `m:ss` (or `h:mm:ss` past an hour) for the
/// history list — same shape as `TeradpsHistory`'s duration column (spec §2).
pub fn format_duration(duration_ms: u64) -> String {
    let total_secs = duration_ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// A fresh temp-file database path per test, so parallel test threads never
/// collide on the same on-disk database. Shared by every history test in
/// this crate — `writer.rs`'s and `pipeline.rs`'s alike.
#[cfg(test)]
pub(crate) fn temp_history_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ShinraMeter-BPSR-test-history-{tag}-{}-{n}.sqlite",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(uid: i64, name: &str) -> PlayerRow {
        PlayerRow {
            uid,
            entity: EntityId::from_display_uid(uid, EntityKind::Player).0 as i64,
            name: name.to_string(),
            class: Some(Class::Stormblade),
            ability_score: Some(1234),
            season_strength: Some(56),
            imagines: [Some(1), None],
            imagine_tiers: [Some(2), None],
            damage: 1_000,
            dps: 100.0,
            share_pct: 50.0,
            crit_pct: 10.0,
            lucky_pct: 5.0,
            hits: 20,
            deaths: 1,
            dead_ms: None,
            skills: Vec::new(),
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            buffs: Vec::new(),
            absorbed_total: 0,
            immune_total: 0,
            shield: None,
        }
    }

    fn sample_snapshot(rows: Vec<PlayerRow>, total_damage: i64) -> Snapshot {
        Snapshot {
            duration_ms: 12_345,
            total_damage,
            total_dps: 999.0,
            total_absorbed: 0,
            total_immune: 0,
            rows,
            encounter: EncounterInfo {
                boss_monster_id: Some(42),
                boss_name: Some("Test Boss"),
                is_boss: true,
                scene_id: Some(7),
                scene_name: Some("Test Scene"),
                scene_boss_name: Some("Test Boss"),
                multi_boss_scene: false,
            },
            local_uid: None,
            capture_alive: true,
        }
    }

    #[test]
    fn record_from_snapshot_carries_every_row() {
        let rows = vec![
            sample_row(1, "Alice"),
            sample_row(2, "Bob"),
            sample_row(3, "Carol"),
        ];
        let snapshot = sample_snapshot(rows, 3_000);

        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        assert_eq!(
            record
                .players
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Alice".to_string(), "Bob".to_string(), "Carol".to_string()]
        );
    }

    #[test]
    fn record_from_snapshot_rejects_an_empty_fight() {
        let snapshot = sample_snapshot(vec![], 0);
        assert!(record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).is_none());
    }

    #[test]
    fn record_from_snapshot_rejects_a_zero_damage_fight() {
        let snapshot = sample_snapshot(vec![sample_row(1, "Alice")], 0);
        assert!(record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).is_none());
    }

    #[test]
    fn record_from_snapshot_keeps_the_supplied_title() {
        let snapshot = sample_snapshot(vec![sample_row(1, "Alice")], 1_000);
        let record = record_from_snapshot(
            &snapshot,
            1_000,
            "My Title".to_string(),
            Some("My Subtitle".to_string()),
        )
        .unwrap();

        assert_eq!(record.title, "My Title");
        assert_eq!(record.subtitle, Some("My Subtitle".to_string()));
    }

    #[test]
    fn to_snapshot_round_trips_the_player_rows() {
        let rows = vec![sample_row(1, "Alice"), sample_row(2, "Bob")];
        let snapshot = sample_snapshot(rows.clone(), 2_000);
        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        let rebuilt = record.to_snapshot();

        assert_eq!(
            rebuilt
                .rows
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>(),
            rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn to_snapshot_leaves_the_borrowed_name_fields_empty() {
        let snapshot = sample_snapshot(vec![sample_row(1, "Alice")], 1_000);
        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        let rebuilt = record.to_snapshot();

        assert!(rebuilt.encounter.boss_name.is_none() && rebuilt.encounter.scene_name.is_none());
    }

    #[test]
    fn to_snapshot_preserves_the_scene_and_boss_ids() {
        let snapshot = sample_snapshot(vec![sample_row(1, "Alice")], 1_000);
        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        let rebuilt = record.to_snapshot();

        assert_eq!(
            (
                rebuilt.encounter.boss_monster_id,
                rebuilt.encounter.scene_id
            ),
            (Some(42), Some(7))
        );
    }

    fn sample_skill(skill_id: i32, damage: i64) -> SkillRow {
        SkillRow {
            skill_id,
            damage,
            share_pct: 60.0,
            crit_pct: 25.0,
            max_crit: damage / 2,
            avg_crit: 1_500.5,
            avg_white: 900.25,
            avg: 1_200.75,
            hits: 8,
            crit_hits: 2,
            hits_per_min: 40.5,
            absorbed_total: 0,
            immune_total: 0,
        }
    }

    #[test]
    fn record_from_snapshot_carries_the_per_skill_breakdown() {
        let mut row = sample_row(1, "Alice");
        row.skills = vec![sample_skill(101, 700), sample_skill(102, 300)];
        let snapshot = sample_snapshot(vec![row], 1_000);

        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        assert_eq!(
            record.players[0]
                .skills
                .iter()
                .map(|s| (s.skill_id, s.damage))
                .collect::<Vec<_>>(),
            vec![(101, 700), (102, 300)]
        );
    }

    /// Issue #222: the breakdown window opened from a historical row draws
    /// `PlayerRow::skills`, so a saved fight has to hand those back field for
    /// field — not just the ids.
    #[test]
    fn to_row_hydrates_the_saved_skill_breakdown() {
        let mut row = sample_row(1, "Alice");
        row.skills = vec![sample_skill(101, 700)];
        let snapshot = sample_snapshot(vec![row], 1_000);
        let record = record_from_snapshot(&snapshot, 1_000, "Title".to_string(), None).unwrap();

        let rebuilt = record.to_snapshot();

        let skill = &rebuilt.rows[0].skills[0];
        assert_eq!(
            (
                skill.skill_id,
                skill.damage,
                skill.max_crit,
                skill.hits,
                skill.crit_hits
            ),
            (101, 700, 350, 8, 2)
        );
        assert_eq!(
            (
                skill.share_pct,
                skill.crit_pct,
                skill.avg_crit,
                skill.avg_white,
                skill.avg,
                skill.hits_per_min
            ),
            (60.0, 25.0, 1_500.5, 900.25, 1_200.75, 40.5)
        );
    }

    #[test]
    fn format_duration_renders_minutes_and_seconds() {
        assert_eq!(format_duration(272_000), "4:32");
    }

    #[test]
    fn format_duration_renders_hours_past_an_hour() {
        assert_eq!(format_duration(3_661_000), "1:01:01");
    }

    #[test]
    fn format_local_time_is_non_empty_for_a_known_epoch() {
        assert!(!format_local_time(1_700_000_000_000).is_empty());
    }

    #[test]
    fn default_retention_matches_the_spec() {
        let policy = RetentionPolicy::default();
        assert_eq!(
            (
                policy.max_encounters,
                policy.max_age_days,
                policy.min_duration_ms
            ),
            (500, 90, 5_000)
        );
    }
}
