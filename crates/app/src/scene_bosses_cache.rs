//! Cross-session scene -> observed-bosses cache persistence (issue #131,
//! widened to a list per scene by issue #150).
//!
//! Mirrors `bpsr_meter::names_cache`'s shape and posture (missing/corrupt
//! file degrades to empty, logged, never a panic), but — unlike that
//! module — lives entirely in this crate rather than `bpsr-meter`. Issue
//! #131 is explicit that the meter crate should stay free of disk I/O:
//! `Meter` only exposes file-oblivious seed/export accessors
//! (`with_scene_bosses`, `set_scene_bosses`, `scene_bosses_for_save`), and
//! this module is where the app crate turns those into an actual file.
//!
//! Every failure mode (missing file, unreadable file, corrupt JSON, an
//! unrecognized `version`) is logged and swallowed rather than propagated
//! as a panic or error: a broken cache must degrade to "no cache", never
//! crash the app.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Bumped whenever the on-disk shape changes. A file with a missing or
/// mismatched `version` is discarded and treated as empty rather than
/// guessing at a migration — see `load`'s doc comment for why this is
/// deliberately *not* how game-patch staleness is handled.
const VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct SceneBossEntry {
    scene_id: u32,
    /// Every boss observed in this scene, in engagement order, most recent
    /// last — `Meter::scene_bosses`' value verbatim. Issue #150 widened this
    /// from a single `monster_id`: one id per scene cannot express a raid
    /// that offers a *choice* of bosses, which is the case the meter has to
    /// stop guessing in. Version 1 files (the single-id shape) are simply
    /// discarded by the `VERSION` check — this project keeps no backward
    /// compatibility, and the map re-learns itself on the next run of each
    /// dungeon anyway.
    monster_ids: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    version: u32,
    entries: Vec<SceneBossEntry>,
}

/// Loads the scene -> observed-bosses map from `path`. A missing file is the
/// expected first-run state and resolves silently to an empty map; a
/// corrupt/unparseable file or a `version` that doesn't match [`VERSION`]
/// both resolve to an empty map too, logged at `warn` — see the module doc
/// comment for why this degrades rather than panicking or erroring.
///
/// issue #131: a game patch changing a dungeon's final boss is deliberately
/// **not** handled by invalidating this file on a build fingerprint.
/// `Meter::recompute_boss` overwrites a scene's entry the next time that
/// dungeon is actually run (see its issue #125 doc comment), so a stale
/// entry self-heals on the next real encounter rather than needing this
/// cache to know anything about the game's build. The user-facing escape
/// hatch for "I don't trust what's cached right now" is the "Forget learned
/// bosses" menu action (`ui.rs`'s `draw_header_menu`), which calls
/// [`forget`] below.
pub fn load(path: &Path) -> HashMap<u32, Vec<u32>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            log::warn!(
                "scene-bosses cache: failed to read {}: {err}",
                path.display()
            );
            return HashMap::new();
        }
    };

    let file = match serde_json::from_slice::<CachedFile>(&bytes) {
        Ok(file) => file,
        Err(err) => {
            log::warn!("scene-bosses cache: corrupt file {}: {err}", path.display());
            return HashMap::new();
        }
    };

    if file.version != VERSION {
        log::warn!(
            "scene-bosses cache: unrecognized version {} in {} (expected {VERSION}); discarding",
            file.version,
            path.display()
        );
        return HashMap::new();
    }

    file.entries
        .into_iter()
        .map(|e| (e.scene_id, e.monster_ids))
        .collect()
}

/// Writes `scene_bosses` to `path`, stamped with the current [`VERSION`].
/// Writes to a sibling `.tmp` file and renames it over `path` — the same
/// crash-safe idiom as `bpsr_meter::names_cache::save` and
/// `settings::save_to` — so a crash or power loss mid-write can never leave
/// a truncated/corrupt cache file behind. Any IO or serialization error is
/// logged at `warn` and otherwise ignored — a failed save must never panic
/// or interrupt the caller.
pub fn save(path: &Path, scene_bosses: &HashMap<u32, Vec<u32>>) {
    let mut entries: Vec<SceneBossEntry> = scene_bosses
        .iter()
        .map(|(&scene_id, monster_ids)| SceneBossEntry {
            scene_id,
            monster_ids: monster_ids.clone(),
        })
        .collect();
    // Deterministic on-disk order purely so two saves of the same map diff
    // quietly — `load` doesn't depend on it.
    entries.sort_by_key(|e| e.scene_id);

    let file = CachedFile {
        version: VERSION,
        entries,
    };
    let json = match serde_json::to_vec_pretty(&file) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("scene-bosses cache: failed to serialize: {err}");
            return;
        }
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!(
            "scene-bosses cache: failed to create {}: {err}",
            parent.display()
        );
        return;
    }

    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp_path, &json) {
        log::warn!(
            "scene-bosses cache: failed to write {}: {err}",
            tmp_path.display()
        );
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        log::warn!(
            "scene-bosses cache: failed to rename {} into {}: {err}",
            tmp_path.display(),
            path.display()
        );
    }
}

/// Deletes the on-disk cache at `path`, if present. Backs the "Forget
/// learned bosses" menu action together with clearing the in-process map
/// (`Pipeline::forget_scene_bosses`). A missing file is not an error —
/// "already forgotten" is success, not failure.
pub fn forget(path: &Path) {
    if let Err(err) = fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "scene-bosses cache: failed to remove {}: {err}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsr_test_support::scratch_path;

    #[test]
    fn round_trip_load_after_save() {
        let path = scratch_path("scene-bosses-round-trip");
        let map = HashMap::from([(1001, vec![103]), (2002, vec![103_108, 103_208])]);
        save(&path, &map);

        let loaded = load(&path);
        assert_eq!(loaded, map);
        // Engagement order is what "the last boss engaged" is read from, so
        // it has to survive the disk round trip (issue #150).
        assert_eq!(loaded[&2002], vec![103_108, 103_208]);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_loads_as_empty_with_no_panic() {
        let path = scratch_path("scene-bosses-missing");
        let loaded = load(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn corrupt_file_loads_as_empty_with_no_panic() {
        let path = scratch_path("scene-bosses-corrupt");
        fs::write(&path, b"{ this is not valid json").unwrap();

        let loaded = load(&path);
        assert!(loaded.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_version_field_is_treated_as_corrupt_and_falls_back_to_empty() {
        let path = scratch_path("scene-bosses-no-version");
        fs::write(
            &path,
            br#"{"entries":[{"scene_id":1001,"monster_ids":[103]}]}"#,
        )
        .unwrap();

        let loaded = load(&path);
        assert!(loaded.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mismatched_version_discards_and_falls_back_to_empty() {
        let path = scratch_path("scene-bosses-bad-version");
        fs::write(
            &path,
            format!(
                r#"{{"version":{},"entries":[{{"scene_id":1001,"monster_ids":[103]}}]}}"#,
                VERSION + 1
            ),
        )
        .unwrap();

        let loaded = load(&path);
        assert!(loaded.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = scratch_path("scene-bosses-no-tmp");
        save(&path, &HashMap::from([(1001, vec![103])]));

        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn forget_removes_an_existing_file() {
        let path = scratch_path("scene-bosses-forget");
        save(&path, &HashMap::from([(1001, vec![103])]));
        assert!(path.exists());

        forget(&path);
        assert!(!path.exists());
    }

    #[test]
    fn forget_of_a_missing_file_is_not_an_error() {
        let path = scratch_path("scene-bosses-forget-missing");
        assert!(!path.exists());
        forget(&path);
        assert!(!path.exists());
    }
}
