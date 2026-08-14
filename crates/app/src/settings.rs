//! Persisted user settings: which stat columns the meter renders (issue #13).
//!
//! Lives entirely at the UI layer — no meter/pipeline involvement. Loaded
//! once at startup and saved on every change from the settings menu.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use crate::ui::{StatColumn, fmt_share, fmt_short};

/// One selectable stat column. Declaration order here is also the
/// canonical left-to-right column order used whenever more than one is
/// enabled, regardless of the order columns were toggled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnKind {
    AbilityScore,
    Damage,
    Dps,
    SharePct,
    CritPct,
    LuckyPct,
    Hits,
}

impl ColumnKind {
    /// Every selectable column, in canonical left-to-right order.
    ///
    /// `AbilityScore` leads the list rather than sitting among the
    /// combat-derived columns: it's a static per-player character stat (a
    /// gear-score snapshot, not something that accrues over the fight like
    /// damage/hits do), so it reads better next to the row's name than
    /// mixed in with `Damage`/`Dps`/etc.
    pub const ALL: [ColumnKind; 7] = [
        ColumnKind::AbilityScore,
        ColumnKind::Damage,
        ColumnKind::Dps,
        ColumnKind::SharePct,
        ColumnKind::CritPct,
        ColumnKind::LuckyPct,
        ColumnKind::Hits,
    ];

    /// Label shown next to this column's checkbox in the settings menu.
    pub fn label(self) -> &'static str {
        match self {
            ColumnKind::AbilityScore => "Ability Score",
            ColumnKind::Damage => "Damage",
            ColumnKind::Dps => "DPS",
            ColumnKind::SharePct => "Share %",
            ColumnKind::CritPct => "Crit %",
            ColumnKind::LuckyPct => "Lucky %",
            ColumnKind::Hits => "Hits",
        }
    }

    /// This column's fixed on-screen width plus the formatter that renders
    /// its value, handed over together in one `StatColumn` so the two can
    /// never be wired up independently — a new `ColumnKind` cannot reserve
    /// space without also saying what gets painted into it, or vice versa.
    ///
    /// `width` is the space this column reserves to its own *left* (issue
    /// #8's anchor scheme), budgeted for the widest text `text` can
    /// produce; `ui`'s `widest_formatted_text_fits_its_column_width_budget`
    /// holds every column here to that budget.
    pub fn spec(self) -> StatColumn {
        match self {
            // `None` (no FIGHT_POINT packet seen yet for this player) is a
            // blank cell, not "0" — a missing reading is not the same as a
            // zero score. `fmt_short`'s ≤7-char budget applies whenever a
            // value is present, so this shares the 56.0 width with
            // `Damage`/`Hits`.
            ColumnKind::AbilityScore => StatColumn {
                width: 56.0,
                text: |row| match row.ability_score {
                    Some(v) => fmt_short(v as i64),
                    None => String::new(),
                },
            },
            ColumnKind::Damage => StatColumn {
                width: 56.0,
                text: |row| fmt_short(row.damage),
            },
            // `fmt_short`'s ≤7 chars plus the 2-char "/s" suffix = ≤9
            // chars, so this column needs more room than the others.
            ColumnKind::Dps => StatColumn {
                width: 76.0,
                text: |row| format!("{}/s", fmt_short(row.dps as i64)),
            },
            ColumnKind::SharePct => StatColumn {
                width: 56.0,
                text: |row| fmt_share(row.share_pct),
            },
            ColumnKind::CritPct => StatColumn {
                width: 56.0,
                text: |row| fmt_share(row.crit_pct),
            },
            ColumnKind::LuckyPct => StatColumn {
                width: 56.0,
                text: |row| fmt_share(row.lucky_pct),
            },
            // `fmt_short` bounds this to ~7 chars regardless of how many
            // hits land, so it shares `Damage`/`Dps`'s width instead of
            // growing without limit like a raw `to_string()` would (it
            // could otherwise overflow this column's fixed-width slot into
            // its neighbor's, since `draw_row`'s painter text is
            // unclipped).
            ColumnKind::Hits => StatColumn {
                width: 56.0,
                text: |row| fmt_short(row.hits as i64),
            },
        }
    }
}

/// User-configurable settings, persisted to `%APPDATA%\shinra-bpsr\settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub visible_columns: Vec<ColumnKind>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            visible_columns: vec![ColumnKind::Damage, ColumnKind::Dps, ColumnKind::SharePct],
        }
    }
}

impl Settings {
    /// Whether `col` is currently enabled.
    pub fn is_visible(&self, col: ColumnKind) -> bool {
        self.visible_columns.contains(&col)
    }

    /// The enabled columns in canonical left-to-right order (see
    /// `ColumnKind::ALL`), independent of the order they were toggled in.
    pub fn ordered_columns(&self) -> Vec<ColumnKind> {
        ColumnKind::ALL
            .into_iter()
            .filter(|c| self.is_visible(*c))
            .collect()
    }

    /// Toggles a column on/off. Refuses to disable the last remaining
    /// visible column, so the row can never end up with nothing to show —
    /// the "all columns disabled" nonsense state guarded against by #13.
    pub fn toggle(&mut self, col: ColumnKind) {
        if self.is_visible(col) {
            if self.visible_columns.len() > 1 {
                self.visible_columns.retain(|c| *c != col);
            }
        } else {
            self.visible_columns.push(col);
        }
    }

    /// Falls back to the default column set if this value is in a nonsense
    /// state (currently: no columns enabled). `toggle` already prevents
    /// reaching this via the settings menu, but a hand-edited or otherwise
    /// malformed settings file could still deserialize into one.
    fn sanitized(self) -> Self {
        if self.visible_columns.is_empty() {
            Self::default()
        } else {
            self
        }
    }
}

/// `%APPDATA%\shinra-bpsr\settings.json`, or `None` if `APPDATA` isn't set
/// (e.g. running outside Windows).
fn settings_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(appdata)
            .join("shinra-bpsr")
            .join("settings.json"),
    )
}

/// Loads settings from `%APPDATA%\shinra-bpsr\settings.json`. Falls back to
/// defaults if `APPDATA` isn't set, the file is missing, or it fails to
/// parse — never panics.
pub fn load() -> Settings {
    match settings_path() {
        Some(path) => load_from(&path),
        None => {
            log::warn!("APPDATA not set; using default settings");
            Settings::default()
        }
    }
}

/// Persists settings to `%APPDATA%\shinra-bpsr\settings.json`. Logs and
/// gives up on any IO error — never panics, never blocks the UI thread on
/// failure.
pub fn save(settings: &Settings) {
    match settings_path() {
        Some(path) => save_to(&path, settings),
        None => log::warn!("APPDATA not set; settings not persisted"),
    }
}

/// Spawns the dedicated settings-writer thread: it owns the settings file
/// and is the only place `save` is called from, keeping the blocking
/// `fs::write` + `fs::rename` off the UI/render thread — the same
/// channel-owning-thread pattern `pipeline::spawn` uses for the meter.
///
/// The UI sends a `Settings` snapshot on every change; the writer coalesces
/// bursts (e.g. several rapid checkbox toggles) by draining the channel down
/// to the newest value before persisting, so a flurry of toggles results in
/// one save of the final state rather than one save per toggle.
pub fn spawn_writer() -> (Sender<Settings>, JoinHandle<()>) {
    spawn_writer_with(save)
}

/// Same as `spawn_writer`, but with the persist step injected — lets tests
/// observe what the writer thread saves without touching the real
/// `%APPDATA%` settings file.
fn spawn_writer_with(
    persist: impl Fn(&Settings) + Send + 'static,
) -> (Sender<Settings>, JoinHandle<()>) {
    let (tx, rx) = unbounded::<Settings>();
    let handle = std::thread::Builder::new()
        .name("settings-writer".to_string())
        .spawn(move || run_writer(rx, persist))
        .expect("failed to spawn the settings-writer thread");
    (tx, handle)
}

/// Blocks on the channel, persisting the newest pending `Settings` each time
/// it wakes up. Returns (and the thread exits) once every `Sender` is
/// dropped.
fn run_writer(rx: Receiver<Settings>, persist: impl Fn(&Settings)) {
    while let Ok(mut settings) = rx.recv() {
        for latest in rx.try_iter() {
            settings = latest;
        }
        persist(&settings);
    }
}

fn load_from(path: &Path) -> Settings {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to read settings at {}: {err}", path.display());
            }
            return Settings::default();
        }
    };
    match serde_json::from_str::<Settings>(&contents) {
        Ok(settings) => settings.sanitized(),
        Err(err) => {
            log::warn!("failed to parse settings at {}: {err}", path.display());
            Settings::default()
        }
    }
}

/// Writes via a temp-file-plus-rename so a crash or power loss mid-write
/// can never leave a half-written file for the next `load` to trip over.
fn save_to(path: &Path, settings: &Settings) {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!(
            "failed to create settings directory {}: {err}",
            parent.display()
        );
        return;
    }
    let json = match serde_json::to_string_pretty(settings) {
        Ok(j) => j,
        Err(err) => {
            log::warn!("failed to serialize settings: {err}");
            return;
        }
    };
    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp_path, json) {
        log::warn!(
            "failed to write settings temp file {}: {err}",
            tmp_path.display()
        );
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        log::warn!("failed to move settings temp file into place: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh path per test (and per call within a test), so parallel test
    /// runs never collide on the same file.
    fn temp_settings_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "shinra-bpsr-test-{tag}-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn round_trip_preserves_settings() {
        let path = temp_settings_path("roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::Hits);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let path = temp_settings_path("missing");
        // Deliberately never written.
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        let path = temp_settings_path("corrupt");
        fs::write(&path, b"not valid json{{{").expect("write corrupt fixture");

        assert_eq!(load_from(&path), Settings::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_creates_missing_parent_directory() {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "shinra-bpsr-test-nested-{}-{n}",
            std::process::id()
        ));
        let path = dir.join("settings.json");

        save_to(&path, &Settings::default());

        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_refuses_to_disable_the_last_visible_column() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Damage],
        };

        settings.toggle(ColumnKind::Damage);

        assert_eq!(settings.visible_columns, vec![ColumnKind::Damage]);
    }

    #[test]
    fn ability_score_is_not_visible_by_default() {
        assert!(!Settings::default().is_visible(ColumnKind::AbilityScore));
    }

    #[test]
    fn ability_score_column_round_trips() {
        let path = temp_settings_path("ability-score-roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn settings_json_without_ability_score_still_deserializes() {
        let path = temp_settings_path("legacy-no-ability-score");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write legacy fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded, Settings::default());
        assert!(!loaded.is_visible(ColumnKind::AbilityScore));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn empty_visible_columns_sanitizes_to_default() {
        let settings = Settings {
            visible_columns: vec![],
        };
        assert_eq!(settings.sanitized(), Settings::default());
    }

    #[test]
    fn loading_a_hand_edited_empty_column_list_falls_back_to_default() {
        let path = temp_settings_path("empty-columns");
        fs::write(&path, br#"{"visible_columns":[]}"#).expect("write fixture");

        assert_eq!(load_from(&path), Settings::default());
        let _ = fs::remove_file(&path);
    }

    // -- spawn_writer / run_writer (the settings-writer thread) ----------

    /// Sending one `Settings` value results in exactly one persist call
    /// with that value — the basic wiring works.
    #[test]
    fn writer_persists_a_sent_settings_value() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Settings>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let (tx, handle) = spawn_writer_with(move |s| seen_clone.lock().unwrap().push(s.clone()));

        let mut settings = Settings::default();
        settings.toggle(ColumnKind::Hits);
        tx.send(settings.clone()).expect("writer thread is alive");

        drop(tx);
        handle.join().expect("writer thread should not panic");

        assert_eq!(*seen.lock().unwrap(), vec![settings]);
    }

    /// A burst of sends made faster than the writer can wake up and drain
    /// must coalesce down to the final value rather than persisting every
    /// intermediate one.
    #[test]
    fn writer_coalesces_a_burst_of_sends_to_the_latest_value() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Settings>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let (tx, handle) = spawn_writer_with(move |s| seen_clone.lock().unwrap().push(s.clone()));

        let mut settings = Settings::default();
        for col in [ColumnKind::CritPct, ColumnKind::LuckyPct, ColumnKind::Hits] {
            settings.toggle(col);
            tx.send(settings.clone()).expect("writer thread is alive");
        }

        drop(tx);
        handle.join().expect("writer thread should not panic");

        // The writer may or may not have woken up between individual sends
        // (that race is exactly what coalescing tolerates), but the very
        // last thing persisted must be the final settings state, and
        // nothing after it.
        assert_eq!(seen.lock().unwrap().last(), Some(&settings));
    }

    /// Dropping every `Sender` closes the channel and the thread exits
    /// cleanly instead of blocking forever.
    #[test]
    fn writer_thread_exits_once_the_sender_is_dropped() {
        let (tx, handle) = spawn_writer_with(|_| {});
        drop(tx);
        handle.join().expect("writer thread should not panic");
    }

    #[test]
    fn ordered_columns_follows_canonical_order_regardless_of_toggle_order() {
        let mut a = Settings {
            visible_columns: vec![ColumnKind::Damage],
        };
        a.toggle(ColumnKind::Hits);
        a.toggle(ColumnKind::CritPct);

        let mut b = Settings {
            visible_columns: vec![ColumnKind::Damage],
        };
        b.toggle(ColumnKind::CritPct);
        b.toggle(ColumnKind::Hits);

        assert_eq!(a.ordered_columns(), b.ordered_columns());
        assert_eq!(
            a.ordered_columns(),
            vec![ColumnKind::Damage, ColumnKind::CritPct, ColumnKind::Hits]
        );
    }
}
