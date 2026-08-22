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

pub mod sqlite;

use std::path::PathBuf;

use bpsr_meter::{Class, EncounterInfo, PlayerRow, Snapshot};

/// Schema version this build writes and reads (spec §5.4). Bumped whenever
/// the DDL in `sqlite::init_schema` changes; an on-disk file stamped with any
/// other `PRAGMA user_version` is renamed aside and replaced with a fresh one
/// rather than migrated (DECISION D11 — there is exactly one schema and zero
/// shipped users of it).
pub const SCHEMA_VERSION: i32 = 1;

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
        Snapshot {
            duration_ms: self.duration_ms,
            total_damage: self.total_damage,
            total_dps: self.total_dps,
            rows: self.players.iter().map(PlayerRecord::to_row).collect(),
            encounter: EncounterInfo {
                boss_monster_id: self.boss_monster_id,
                is_boss: self.is_boss,
                scene_id: self.scene_id,
                ..EncounterInfo::default()
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(uid: i64, name: &str) -> PlayerRow {
        PlayerRow {
            uid,
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
        }
    }

    fn sample_snapshot(rows: Vec<PlayerRow>, total_damage: i64) -> Snapshot {
        Snapshot {
            duration_ms: 12_345,
            total_damage,
            total_dps: 999.0,
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
