//! Session-bundle export ("Export session bundle" header-menu item,
//! `ui::draw_header_menu`): hands a user's whole session over as one folder
//! — logs, the packet-inspection dump ring if `SHINRA_INSPECT` was on this
//! session, and `settings.json` — plus a `manifest.json` describing what's
//! in it, so an agent can find every bug in the session without the
//! maintainer's help.
//!
//! `history.sqlite` is deliberately never part of the bundle: it holds
//! plaintext party-member names (`crate::history`'s module doc comment),
//! and handing that over as part of a routine bug report is not something
//! this export does silently — [`Manifest::excluded`] says so explicitly,
//! rather than a reader having to notice the file is simply missing and
//! guess why.
//!
//! Mirrors `logging::export_logs_to`'s shape (best-effort per file: a
//! partially-written bundle is more useful to whoever's debugging than
//! silently deleting it) but writes a directory instead of one concatenated
//! file, since a bundle has more than one *kind* of artifact to hand over.
//! A directory rather than a zip: nothing in this dependency tree writes
//! zip archives today, and adding one is out of scope for this feature.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The default folder name the "Export session bundle" menu item seeds its
/// save dialog with — the user can still rename it before saving, same as
/// `logging::EXPORT_DEFAULT_FILENAME`.
pub(crate) const EXPORT_BUNDLE_DEFAULT_DIRNAME: &str = "ShinraMeter-BPSR-session-bundle";

/// Why `history.sqlite` never appears in a bundle — surfaced verbatim in
/// `manifest.json`'s `excluded` list so a reader never has to guess whether
/// the omission was deliberate or a bug in the export.
pub(crate) const HISTORY_EXCLUSION_REASON: &str =
    "holds plaintext party member names; never leaves this machine as part of a bug report";

/// One entry in `manifest.json`'s `excluded` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExcludedFile {
    pub name: String,
    pub reason: String,
}

/// `manifest.json`'s shape: enough for an agent staring at a bare folder to
/// know what session produced it, whether packet-inspection diagnostics
/// were on (so a missing dump file is expected, not a bug in the export),
/// and how much to trust the dump if it is present.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Manifest {
    pub session_id: String,
    pub app_version: String,
    /// RFC 3339, UTC — when this session's process started. Derived from
    /// `logging::session_id`'s `<pid>-<unix_secs>` shape via
    /// [`started_at_from_session_id`].
    pub started_at: String,
    pub inspect_enabled: bool,
    /// Total byte budget across every dump-ring chunk
    /// (`dump::max_total_ring_bytes`) — `0` when `inspect_enabled` is
    /// `false`, since no ring exists to budget.
    pub dump_byte_budget: u64,
    /// How many dump records were dropped because the writer thread fell
    /// behind, if that count could be determined — `None` when
    /// `inspect_enabled` is `false`, or when it's `true` but the count
    /// genuinely isn't available (mirrors `dump::DumpWriter::shutdown`'s
    /// own "count if available" reporting).
    pub dropped_records: Option<u64>,
    pub excluded: Vec<ExcludedFile>,
}

/// Builds `manifest.json`'s contents — pure, so it's unit-tested without
/// touching a real session, the filesystem, or the global logger.
pub fn build_manifest(
    session_id: &str,
    app_version: &str,
    started_at_unix_secs: u64,
    inspect_enabled: bool,
    dump_byte_budget: u64,
    dropped_records: Option<u64>,
) -> Manifest {
    Manifest {
        session_id: session_id.to_string(),
        app_version: app_version.to_string(),
        started_at: format_unix_secs(started_at_unix_secs),
        inspect_enabled,
        dump_byte_budget: if inspect_enabled { dump_byte_budget } else { 0 },
        dropped_records: if inspect_enabled {
            dropped_records
        } else {
            None
        },
        excluded: vec![ExcludedFile {
            name: "history.sqlite".to_string(),
            reason: HISTORY_EXCLUSION_REASON.to_string(),
        }],
    }
}

/// `secs` (Unix seconds) formatted as RFC 3339 UTC. `DateTime::from_
/// timestamp` only fails for a value outside `chrono`'s representable
/// range — never true for a real `SystemTime`-derived value — so the
/// epoch is a purely defensive fallback, not a path any real caller hits.
fn format_unix_secs(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch is representable"))
        .to_rfc3339()
}

/// `logging::session_id()`'s unix-seconds half, parsed back out for
/// [`build_manifest`]'s `started_at` — that function is `<pid>-<unix_secs>`,
/// and this is the one place outside `logging` that needs the second half
/// alone. `0` (the epoch) for a malformed id — defensive only, since every
/// real session id is produced by `logging::session_id` itself.
pub fn started_at_from_session_id(session_id: &str) -> u64 {
    session_id
        .rsplit_once('-')
        .and_then(|(_, secs)| secs.parse().ok())
        .unwrap_or(0)
}

/// Every ring chunk for `dump_path` currently on disk, oldest first, with
/// `dump_path` itself (the live chunk) last — if it exists. Mirrors
/// `dump_format::load_dump`'s own chronological order, so a bundle's dump
/// files read back the same way that reader expects. Touches the
/// filesystem (existence checks only) — unlike [`build_manifest`] and
/// [`bundle_entries`], this isn't a pure function.
pub fn dump_ring_parts(dump_path: &Path) -> Vec<PathBuf> {
    let mut numbered = Vec::new();
    let mut n = 1;
    loop {
        let candidate = bpsr_protocol::dump_format::numbered_sibling(dump_path, n);
        if !candidate.exists() {
            break;
        }
        numbered.push(candidate);
        n += 1;
    }
    // Collected ascending by `n` (newest-rotated first); reversed so the
    // oldest (highest `n`) chunk comes first, matching `dump_format::
    // load_dump`'s read order.
    numbered.reverse();
    if dump_path.exists() {
        numbered.push(dump_path.to_path_buf());
    }
    numbered
}

/// Maps every source file a bundle export should collect to the name it
/// gets inside the bundle directory — each source's own basename, since
/// none of `log_parts`/`dump_parts`/`settings_path` can collide on disk
/// (they're already-distinct files today, and nothing about the bundle
/// renames them). A source with no file name (e.g. `/`) is silently
/// skipped rather than panicking — pure and side-effect-free (no existence
/// check, no IO), so it's unit-tested with made-up paths that don't need to
/// exist on disk.
pub fn bundle_entries(
    log_parts: &[PathBuf],
    dump_parts: &[PathBuf],
    settings_path: Option<&Path>,
) -> Vec<(String, PathBuf)> {
    log_parts
        .iter()
        .map(PathBuf::as_path)
        .chain(dump_parts.iter().map(PathBuf::as_path))
        .chain(settings_path)
        .filter_map(|path| {
            path.file_name()
                .map(|name| (name.to_string_lossy().into_owned(), path.to_path_buf()))
        })
        .collect()
}

/// Writes the whole bundle: creates `dest_dir` (and its parents), copies
/// every `(name, source)` pair `entries` names into `dest_dir/name`, and
/// writes `manifest.json`. Best-effort per file, like
/// `logging::export_logs_to` — a source that can't be copied (e.g. inspect
/// was on but the dump file hasn't been created yet, or a permission
/// error) is skipped with a warning rather than aborting the whole export,
/// since a partial bundle is still useful to whoever's debugging it.
/// `Err` only when `dest_dir` itself can't be created or `manifest.json`
/// can't be written — those leave nothing worth handing over at all.
pub fn export_bundle_to(
    dest_dir: &Path,
    entries: &[(String, PathBuf)],
    manifest: &Manifest,
) -> io::Result<()> {
    fs::create_dir_all(dest_dir)?;
    for (name, source) in entries {
        let dest = dest_dir.join(name);
        if let Err(err) = fs::copy(source, &dest) {
            log::warn!(
                "session bundle: failed to copy {} to {}: {err}",
                source.display(),
                dest.display()
            );
        }
    }
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    fs::write(dest_dir.join("manifest.json"), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- build_manifest -------------------------------------------------

    #[test]
    fn build_manifest_reports_every_field_when_inspect_was_on() {
        let manifest = build_manifest(
            "1234-1700000000",
            "0.2.6",
            1_700_000_000,
            true,
            512 * 1024 * 1024,
            Some(7),
        );
        assert_eq!(manifest.session_id, "1234-1700000000");
        assert_eq!(manifest.app_version, "0.2.6");
        assert_eq!(manifest.started_at, "2023-11-14T22:13:20+00:00");
        assert!(manifest.inspect_enabled);
        assert_eq!(manifest.dump_byte_budget, 512 * 1024 * 1024);
        assert_eq!(manifest.dropped_records, Some(7));
        assert_eq!(manifest.excluded.len(), 1);
        assert_eq!(manifest.excluded[0].name, "history.sqlite");
        assert_eq!(manifest.excluded[0].reason, HISTORY_EXCLUSION_REASON);
    }

    /// When inspect was off, the byte budget and dropped count must not
    /// report numbers that imply a dump ring exists — there is none.
    #[test]
    fn build_manifest_zeroes_dump_fields_when_inspect_was_off() {
        let manifest = build_manifest(
            "1234-1700000000",
            "0.2.6",
            1_700_000_000,
            false,
            512 * 1024 * 1024,
            Some(7),
        );
        assert!(!manifest.inspect_enabled);
        assert_eq!(manifest.dump_byte_budget, 0);
        assert_eq!(manifest.dropped_records, None);
    }

    #[test]
    fn build_manifest_reports_unavailable_dropped_count_as_none() {
        let manifest = build_manifest("1234-1700000000", "0.2.6", 1_700_000_000, true, 100, None);
        assert_eq!(manifest.dropped_records, None);
    }

    #[test]
    fn build_manifest_always_lists_history_sqlite_as_excluded() {
        let manifest = build_manifest("1-1", "0.0.0", 0, false, 0, None);
        assert_eq!(manifest.excluded.len(), 1);
        assert_eq!(manifest.excluded[0].name, "history.sqlite");
    }

    // -- started_at_from_session_id --------------------------------------

    #[test]
    fn started_at_from_session_id_parses_the_unix_seconds_half() {
        assert_eq!(started_at_from_session_id("4242-1700000000"), 1_700_000_000);
    }

    #[test]
    fn started_at_from_session_id_falls_back_to_zero_for_a_malformed_id() {
        assert_eq!(started_at_from_session_id("not-a-session-id-42"), 42);
        assert_eq!(started_at_from_session_id("nodash"), 0);
    }

    // -- bundle_entries (pure) --------------------------------------------

    #[test]
    fn bundle_entries_maps_every_source_to_its_own_basename() {
        let log_parts = vec![
            PathBuf::from("/logs/ShinraMeter-BPSR.log.1"),
            PathBuf::from("/logs/ShinraMeter-BPSR.log"),
        ];
        let dump_parts = vec![
            PathBuf::from("/inspect/dump-1-2.jsonl.1"),
            PathBuf::from("/inspect/dump-1-2.jsonl"),
        ];
        let settings_path = PathBuf::from("/appdata/settings.json");

        let entries = bundle_entries(&log_parts, &dump_parts, Some(&settings_path));

        assert_eq!(
            entries,
            vec![
                (
                    "ShinraMeter-BPSR.log.1".to_string(),
                    PathBuf::from("/logs/ShinraMeter-BPSR.log.1")
                ),
                (
                    "ShinraMeter-BPSR.log".to_string(),
                    PathBuf::from("/logs/ShinraMeter-BPSR.log")
                ),
                (
                    "dump-1-2.jsonl.1".to_string(),
                    PathBuf::from("/inspect/dump-1-2.jsonl.1")
                ),
                (
                    "dump-1-2.jsonl".to_string(),
                    PathBuf::from("/inspect/dump-1-2.jsonl")
                ),
                (
                    "settings.json".to_string(),
                    PathBuf::from("/appdata/settings.json")
                ),
            ]
        );
    }

    #[test]
    fn bundle_entries_omits_settings_when_none() {
        let entries = bundle_entries(&[PathBuf::from("/a.log")], &[], None);
        assert_eq!(
            entries,
            vec![("a.log".to_string(), PathBuf::from("/a.log"))]
        );
    }

    #[test]
    fn bundle_entries_is_empty_for_no_sources() {
        assert!(bundle_entries(&[], &[], None).is_empty());
    }

    // -- dump_ring_parts (touches disk) ------------------------------------

    fn temp_dump_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-test-{tag}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn dump_ring_parts_orders_oldest_chunk_first_and_live_file_last() {
        let path = temp_dump_path("ring");
        let dot1 = bpsr_protocol::dump_format::numbered_sibling(&path, 1);
        let dot2 = bpsr_protocol::dump_format::numbered_sibling(&path, 2);
        fs::write(&dot2, "oldest").unwrap();
        fs::write(&dot1, "middle").unwrap();
        fs::write(&path, "newest").unwrap();

        let parts = dump_ring_parts(&path);

        assert_eq!(parts, vec![dot2.clone(), dot1.clone(), path.clone()]);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&dot1);
        let _ = fs::remove_file(&dot2);
    }

    #[test]
    fn dump_ring_parts_is_empty_when_nothing_exists_yet() {
        let path = temp_dump_path("nothing-yet");
        assert!(dump_ring_parts(&path).is_empty());
    }

    // -- export_bundle_to (touches disk) -----------------------------------

    #[test]
    fn export_bundle_to_copies_every_entry_and_writes_the_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let source = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-source-{}.log",
            std::process::id()
        ));
        fs::write(&source, b"log contents").unwrap();

        let manifest = build_manifest("1-1700000000", "0.2.6", 1_700_000_000, false, 0, None);
        let entries = vec![("session.log".to_string(), source.clone())];

        export_bundle_to(&dir, &entries, &manifest).unwrap();

        assert_eq!(fs::read(dir.join("session.log")).unwrap(), b"log contents");
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains("\"session_id\": \"1-1700000000\""));
        assert!(manifest_json.contains("history.sqlite"));

        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_bundle_to_skips_a_missing_source_but_still_writes_the_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-missing-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let missing = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-does-not-exist-{}.jsonl",
            std::process::id()
        ));
        let manifest = build_manifest("1-1700000000", "0.2.6", 1_700_000_000, true, 100, Some(0));
        let entries = vec![("dump.jsonl".to_string(), missing)];

        export_bundle_to(&dir, &entries, &manifest).unwrap();

        assert!(!dir.join("dump.jsonl").exists());
        assert!(dir.join("manifest.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
