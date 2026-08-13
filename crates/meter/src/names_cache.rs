//! Cross-session uid -> (name, class) cache persistence (issue #12, track 1).
//!
//! This module only knows how to (de)serialize a caller-supplied `Path`; it
//! never picks or hardcodes a location (no Windows-specific paths, no
//! `directories` crate) — that's the app crate's job. Every failure mode
//! (missing file, unreadable file, corrupt JSON, unwritable directory) is
//! logged and swallowed rather than propagated as a panic or error: a broken
//! cache must degrade to "no cache", never crash the app.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::event::Class;

/// Hard cap on persisted entries, enforced on save. Bounds the file size
/// across months of play; `save`'s caller (see `Meter::names_for_save`) is
/// expected to order entries most-recently-used first so the cap evicts the
/// least-recently-used (LRU-ish) entries rather than an arbitrary subset.
pub const MAX_CACHED_NAMES: usize = 2000;

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    uid: i64,
    name: Option<String>,
    class: Option<Class>,
}

/// A loaded uid -> (name, class) cache, in on-disk order (most-recently-used
/// first — see [`load`]'s docs). Named so `Meter::with_names_cache`'s
/// signature (and clippy) don't have to spell the nested tuple out.
pub type LoadedNames = Vec<(i64, (Option<String>, Option<Class>))>;

/// Loads the uid -> (name, class) cache from `path`, preserving on-disk
/// order (most-recently-used first, matching `save`'s ordering contract) —
/// callers that reconstruct recency (see `Meter::with_names_cache`) depend on
/// this order rather than any incidental iteration order. A missing file is
/// the expected first-run state and resolves silently to an empty vec; any
/// other read or parse failure resolves to an empty vec too, logged at `warn`.
pub fn load(path: &Path) -> LoadedNames {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::warn!("names cache: failed to read {}: {err}", path.display());
            return Vec::new();
        }
    };

    match serde_json::from_slice::<Vec<CachedEntry>>(&bytes) {
        Ok(entries) => entries
            .into_iter()
            .map(|e| (e.uid, (e.name, e.class)))
            .collect(),
        Err(err) => {
            log::warn!("names cache: corrupt file {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Writes `names` to `path`, capped at [`MAX_CACHED_NAMES`] entries (extras
/// beyond the cap, i.e. the tail of `names`, are dropped — callers should
/// order most-recently-used first). Writes to a sibling `.tmp` file and
/// renames it over `path` so a crash or power-loss mid-write can never leave
/// a truncated/corrupt cache file behind. Any IO or serialization error is
/// logged at `warn` and otherwise ignored — a failed save must never panic
/// or interrupt the caller.
pub fn save(path: &Path, names: &[(i64, Option<String>, Option<Class>)]) {
    let entries: Vec<CachedEntry> = names
        .iter()
        .take(MAX_CACHED_NAMES)
        .map(|(uid, name, class)| CachedEntry {
            uid: *uid,
            name: name.clone(),
            class: *class,
        })
        .collect();

    let json = match serde_json::to_vec_pretty(&entries) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("names cache: failed to serialize: {err}");
            return;
        }
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!("names cache: failed to create {}: {err}", parent.display());
        return;
    }

    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp_path, &json) {
        log::warn!("names cache: failed to write {}: {err}", tmp_path.display());
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        log::warn!(
            "names cache: failed to rename {} into {}: {err}",
            tmp_path.display(),
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
        let path = scratch_path("round-trip");
        let names = vec![
            (1, Some("Alice".to_string()), Some(Class::Marksman)),
            (2, None, Some(Class::FrostMage)),
            (3, Some("Carol".to_string()), None),
        ];
        save(&path, &names);

        let loaded = load(&path);
        assert_eq!(
            loaded,
            vec![
                (1, (Some("Alice".to_string()), Some(Class::Marksman))),
                (2, (None, Some(Class::FrostMage))),
                (3, (Some("Carol".to_string()), None)),
            ]
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_loads_as_empty_with_no_panic() {
        let path = scratch_path("missing");
        let loaded = load(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn corrupt_file_loads_as_empty_with_no_panic() {
        let path = scratch_path("corrupt");
        fs::write(&path, b"{ this is not valid json").unwrap();

        let loaded = load(&path);
        assert!(loaded.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_caps_output_at_max_cached_names() {
        let path = scratch_path("cap");
        let names: Vec<(i64, Option<String>, Option<Class>)> = (0..MAX_CACHED_NAMES + 500)
            .map(|i| (i as i64, None, None))
            .collect();
        save(&path, &names);

        let loaded = load(&path);
        assert_eq!(loaded.len(), MAX_CACHED_NAMES);
        // The cap keeps the front of the slice (the caller's
        // most-recently-used ordering) and drops the tail.
        assert_eq!(loaded[0].0, 0);
        assert!(
            !loaded
                .iter()
                .any(|(uid, _)| *uid == MAX_CACHED_NAMES as i64 + 100)
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = scratch_path("no-tmp");
        save(&path, &[(1, Some("X".to_string()), None)]);

        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        let _ = fs::remove_file(&path);
    }
}
