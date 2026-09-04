//! Session-bundle export ("Export session bundle" header-menu item,
//! `ui::draw_header_menu`): hands a user's whole session over as one folder
//! — logs, the packet-inspection dump ring if `SHINRA_INSPECT` was on this
//! session, and `settings.json` — plus a `manifest.json` describing what's
//! in it, so an agent can find every bug in the session without the
//! maintainer's help.
//!
//! The raw `history.sqlite` is deliberately never part of the bundle: it
//! holds plaintext party-member names (`crate::history`'s module doc
//! comment), and handing that over as part of a routine bug report is not
//! something this export does silently — [`Manifest::excluded`] says so
//! explicitly, rather than a reader having to notice the file is simply
//! missing and guess why. A sanitized copy (issue #347,
//! `crate::history::sanitize::sanitize_copy`, `history.sanitized.sqlite`)
//! with names replaced by stable pseudonyms is included in its place —
//! see [`Manifest::sanitized_history_included`].
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

/// The one file name a bundle never contains. [`build_manifest`] names it
/// in [`Manifest::excluded`] *and* [`bundle_entries`] drops any source that
/// carries it, so the manifest's claim is enforced by the code that builds
/// the bundle rather than resting on every caller remembering not to hand
/// the history database over.
pub(crate) const HISTORY_FILE_NAME: &str = "history.sqlite";

/// The name a sanitized history copy (issue #347, `crate::history::sanitize`)
/// gets inside the bundle directory — never [`HISTORY_FILE_NAME`] itself, so
/// the "a bundle never contains `history.sqlite`" guarantee
/// [`bundle_entries`] enforces for its own sources stays true regardless of
/// this file sitting right next to it.
pub(crate) const SANITIZED_HISTORY_FILE_NAME: &str = "history.sanitized.sqlite";

/// Why the *raw* `history.sqlite` never appears in a bundle — surfaced
/// verbatim in `manifest.json`'s `excluded` list so a reader never has to
/// guess whether the omission was deliberate or a bug in the export. A
/// sanitized copy with real names replaced by stable pseudonyms
/// ([`SANITIZED_HISTORY_FILE_NAME`]) is included in its place when history
/// export is requested and a history database exists — see
/// [`Manifest::sanitized_history_included`].
pub(crate) const HISTORY_EXCLUSION_REASON: &str = "holds plaintext party member names; the raw file never leaves this machine — a sanitized \
     copy with names replaced by stable pseudonyms is included instead, see sanitized_history_included";

/// Why the dump ring is left out of a bundle when `settings.dump_sanitize`
/// was off for this session (issue #346) — surfaced in `manifest.json`'s
/// `excluded` list the same way [`HISTORY_EXCLUSION_REASON`] is, so a
/// missing dump is a documented decision rather than something a reader has
/// to notice and guess about. A dump written with sanitization off holds
/// raw player names/ids the same way `history.sqlite` does, and there is no
/// sanitized-copy fallback for it the way there is for history — it simply
/// never leaves this machine as part of a bug report.
pub(crate) const DUMP_UNSANITIZED_EXCLUSION_REASON: &str = "written with dump_sanitize off; holds raw player names/ids — never leaves this machine as part of a bug report";

/// The name [`build_manifest`] lists in `excluded` for the dump ring when
/// [`DUMP_UNSANITIZED_EXCLUSION_REASON`] applies — not a single on-disk
/// file name (the ring can be several numbered chunks), just what a reader
/// of `manifest.json` sees.
const DUMP_EXCLUDED_NAME: &str = "inspect dump ring";

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
    /// Whether this session's dump writer was sanitizing on write (issue
    /// #346, `settings::Settings::dump_sanitize`) — `false` (and the dump
    /// ring excluded, see [`DUMP_UNSANITIZED_EXCLUSION_REASON`]) whenever
    /// `inspect_enabled` is `false` too, since there is no dump to speak
    /// of either way.
    pub dump_sanitized: bool,
    /// How many records the sanitizer (issue #346) rejected and left out of
    /// the dump, if that count could be determined — `None` when
    /// `inspect_enabled` is `false`, or when it's `true` but the count
    /// genuinely isn't available. Mirrors [`dropped_records`](Self::dropped_records)'s
    /// "count if available" shape, but for records the sanitizer judged
    /// unsafe rather than ones the writer thread fell behind on.
    pub sanitized_out_records: Option<u64>,
    pub excluded: Vec<ExcludedFile>,
    /// Files the export was told to collect but couldn't copy — the name
    /// each *would* have had inside the bundle. Empty when everything
    /// landed. Filled in by [`export_bundle_to`] rather than
    /// [`build_manifest`], since only the copy loop knows: a dump-ring
    /// chunk listed by [`dump_ring_parts`] can be rotated away by the live
    /// writer thread between the listing and the copy, and a bundle that
    /// silently came up a file short is exactly the thing a reader must not
    /// have to guess at.
    pub missing: Vec<String>,
    /// Whether [`SANITIZED_HISTORY_FILE_NAME`] was written into this bundle
    /// (issue #347) — `false` when the caller asked to skip history
    /// entirely, when no history database exists yet (a fresh install), or
    /// when sanitizing it failed. Filled in by [`export_bundle_to`] for the
    /// same reason [`Manifest::missing`] is: only the copy/sanitize step
    /// knows.
    pub sanitized_history_included: bool,
}

/// The packet-inspection/dump-ring half of `manifest.json`'s fields,
/// grouped into one value so [`build_manifest`] doesn't take one parameter
/// per field (clippy's `too_many_arguments`) — every field here mirrors the
/// identically-named [`Manifest`] field it fills in.
#[derive(Debug, Clone, Copy, Default)]
pub struct DumpStatus {
    pub inspect_enabled: bool,
    pub dump_byte_budget: u64,
    pub dropped_records: Option<u64>,
    pub dump_sanitized: bool,
    pub sanitized_out_records: Option<u64>,
}

/// Builds `manifest.json`'s contents — pure, so it's unit-tested without
/// touching a real session, the filesystem, or the global logger.
pub fn build_manifest(
    session_id: &str,
    app_version: &str,
    started_at_unix_secs: u64,
    dump: DumpStatus,
) -> Manifest {
    let DumpStatus {
        inspect_enabled,
        dump_byte_budget,
        dropped_records,
        dump_sanitized,
        sanitized_out_records,
    } = dump;
    let mut excluded = vec![ExcludedFile {
        name: HISTORY_FILE_NAME.to_string(),
        reason: HISTORY_EXCLUSION_REASON.to_string(),
    }];
    if inspect_enabled && !dump_sanitized {
        excluded.push(ExcludedFile {
            name: DUMP_EXCLUDED_NAME.to_string(),
            reason: DUMP_UNSANITIZED_EXCLUSION_REASON.to_string(),
        });
    }
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
        dump_sanitized: inspect_enabled && dump_sanitized,
        sanitized_out_records: if inspect_enabled {
            sanitized_out_records
        } else {
            None
        },
        excluded,
        // Only `export_bundle_to`'s copy loop can know this.
        missing: Vec::new(),
        // Only `export_bundle_to`'s sanitize step can know this.
        sanitized_history_included: false,
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
/// filesystem — unlike [`build_manifest`] and [`bundle_entries`], this
/// isn't a pure function.
///
/// Enumeration is `dump_format::ring_siblings`, the one implementation the
/// writer (`crate::dump`) and the reader (`dump_format::load_dump`) also
/// use: a bundle must hand over exactly the chunks those two consider part
/// of the ring, gap in the numbering included.
pub fn dump_ring_parts(dump_path: &Path) -> Vec<PathBuf> {
    // `ring_siblings` is ascending by `n` (`.1`, the newest rotated chunk,
    // first); reversed so the oldest (highest `n`) chunk comes first,
    // matching `dump_format::load_dump`'s read order.
    let mut parts: Vec<PathBuf> = bpsr_protocol::dump_format::ring_siblings(dump_path)
        .into_iter()
        .rev()
        .map(|(_, chunk)| chunk)
        .collect();
    if dump_path.exists() {
        parts.push(dump_path.to_path_buf());
    }
    parts
}

/// Maps every source file a bundle export should collect to the name it
/// gets inside the bundle directory — each source's own basename, since
/// none of `log_parts`/`dump_parts`/`settings_path` can collide on disk
/// (they're already-distinct files today, and nothing about the bundle
/// renames them). A source with no file name (e.g. `/`) is silently
/// skipped rather than panicking — pure and side-effect-free (no existence
/// check, no IO), so it's unit-tested with made-up paths that don't need to
/// exist on disk.
///
/// A source named [`HISTORY_FILE_NAME`] is dropped here (with a warning),
/// whatever directory it came from: `manifest.json` states that file is
/// excluded, and this is what makes that statement true no matter how a
/// caller assembled `log_parts`/`dump_parts`/`settings_path`. Enforcing it
/// at the single point every source funnels through beats trusting each
/// call site.
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
        .filter(|(name, path)| {
            if name == HISTORY_FILE_NAME {
                log::warn!(
                    "session bundle: refusing to include {} — {HISTORY_EXCLUSION_REASON}",
                    path.display()
                );
                return false;
            }
            true
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
///
/// Every skipped entry's bundle name is recorded in the written manifest's
/// [`Manifest::missing`] list *and* returned, so neither a reader of the
/// folder nor the caller's status line has to diff `entries` against the
/// directory to notice. That matters most for the dump ring: the writer
/// thread keeps rotating while this copies, so a chunk
/// [`dump_ring_parts`] listed can be renamed out from under `fs::copy`
/// milliseconds later, and "the bundle is one chunk short" is a very
/// different thing to hand a debugger than "the ring was that short".
///
/// `history_source`, when `Some` and `include_history` is `true`, is run
/// through `crate::history::sanitize::sanitize_copy` (issue #347) into
/// `dest_dir/`[`SANITIZED_HISTORY_FILE_NAME`] — the one way `history.sqlite`
/// data ever reaches a bundle, real names replaced by stable pseudonyms.
/// `include_history` lets a caller skip it outright when there is no
/// history to hand over — the UI passes `Settings::history_enabled`, so a
/// user who has turned history off never has it sanitized into a bundle
/// either. A missing source file or a sanitize failure is logged and left
/// out of the bundle, same as any other best-effort entry — it does not
/// fail the whole export.
pub fn export_bundle_to(
    dest_dir: &Path,
    entries: &[(String, PathBuf)],
    manifest: &Manifest,
    history_source: Option<&Path>,
    include_history: bool,
) -> io::Result<Vec<String>> {
    fs::create_dir_all(dest_dir)?;
    let mut missing = Vec::new();
    for (name, source) in entries {
        let dest = dest_dir.join(name);
        if let Err(err) = fs::copy(source, &dest) {
            log::warn!(
                "session bundle: failed to copy {} to {}: {err}",
                source.display(),
                dest.display()
            );
            missing.push(name.clone());
        }
    }

    let sanitized_history_included = match (include_history, history_source) {
        (true, Some(history_source)) => {
            let dest = dest_dir.join(SANITIZED_HISTORY_FILE_NAME);
            match crate::history::sanitize::sanitize_copy(history_source, &dest) {
                Ok(report) => {
                    log::info!(
                        "session bundle: sanitized {} encounter(s), {} player row(s) into {}",
                        report.encounters,
                        report.players_remapped,
                        dest.display()
                    );
                    true
                }
                Err(err) => {
                    log::warn!(
                        "session bundle: failed to sanitize {} into {}: {err}",
                        history_source.display(),
                        dest.display()
                    );
                    false
                }
            }
        }
        _ => false,
    };

    // The manifest handed in was built before the copies ran, so it can't
    // know what didn't land: written out with this run's failures folded
    // in, leaving the caller's copy untouched.
    let manifest = Manifest {
        missing: missing.clone(),
        sanitized_history_included,
        ..manifest.clone()
    };
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    fs::write(dest_dir.join("manifest.json"), json)?;
    Ok(missing)
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
            DumpStatus {
                inspect_enabled: true,
                dump_byte_budget: 512 * 1024 * 1024,
                dropped_records: Some(7),
                dump_sanitized: true,
                sanitized_out_records: Some(3),
            },
        );
        assert_eq!(manifest.session_id, "1234-1700000000");
        assert_eq!(manifest.app_version, "0.2.6");
        assert_eq!(manifest.started_at, "2023-11-14T22:13:20+00:00");
        assert!(manifest.inspect_enabled);
        assert_eq!(manifest.dump_byte_budget, 512 * 1024 * 1024);
        assert_eq!(manifest.dropped_records, Some(7));
        assert!(manifest.dump_sanitized);
        assert_eq!(manifest.sanitized_out_records, Some(3));
        assert_eq!(manifest.excluded.len(), 1);
        assert_eq!(manifest.excluded[0].name, "history.sqlite");
        assert_eq!(manifest.excluded[0].reason, HISTORY_EXCLUSION_REASON);
    }

    /// Issue #346: when a session's dump writer was not sanitizing on
    /// write, the dump ring must show up in `excluded` — a raw dump has no
    /// sanitized fallback the way `history.sqlite` does.
    #[test]
    fn build_manifest_excludes_the_dump_ring_when_not_sanitized() {
        let manifest = build_manifest(
            "1234-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: true,
                dump_byte_budget: 512 * 1024 * 1024,
                dropped_records: Some(7),
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        assert!(!manifest.dump_sanitized);
        assert_eq!(manifest.excluded.len(), 2);
        assert_eq!(manifest.excluded[1].name, "inspect dump ring");
        assert_eq!(
            manifest.excluded[1].reason,
            DUMP_UNSANITIZED_EXCLUSION_REASON
        );
    }

    /// When inspect was off, the byte budget and dropped count must not
    /// report numbers that imply a dump ring exists — there is none.
    #[test]
    fn build_manifest_zeroes_dump_fields_when_inspect_was_off() {
        let manifest = build_manifest(
            "1234-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 512 * 1024 * 1024,
                dropped_records: Some(7),
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        assert!(!manifest.inspect_enabled);
        assert_eq!(manifest.dump_byte_budget, 0);
        assert_eq!(manifest.dropped_records, None);
    }

    #[test]
    fn build_manifest_reports_unavailable_dropped_count_as_none() {
        let manifest = build_manifest(
            "1234-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: true,
                dump_byte_budget: 100,
                dropped_records: None,
                dump_sanitized: true,
                sanitized_out_records: None,
            },
        );
        assert_eq!(manifest.dropped_records, None);
    }

    #[test]
    fn build_manifest_always_lists_history_sqlite_as_excluded() {
        let manifest = build_manifest(
            "1-1",
            "0.0.0",
            0,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        assert_eq!(manifest.excluded.len(), 1);
        assert_eq!(manifest.excluded[0].name, "history.sqlite");
    }

    // -- started_at_from_session_id --------------------------------------

    #[test]
    fn started_at_from_session_id_parses_the_unix_seconds_half() {
        assert_eq!(started_at_from_session_id("4242-1700000000"), 1_700_000_000);
    }

    /// The parse is "whatever follows the *last* dash", so an id that
    /// isn't shaped like `<pid>-<secs>` at all still yields its trailing
    /// numeric segment rather than the epoch. Documented because it's the
    /// reason the fallback below needs an id with no numeric tail to
    /// exercise it.
    #[test]
    fn started_at_from_session_id_parses_a_trailing_numeric_segment_of_an_odd_id() {
        assert_eq!(started_at_from_session_id("not-a-session-id-42"), 42);
    }

    #[test]
    fn started_at_from_session_id_falls_back_to_zero_without_a_numeric_tail() {
        assert_eq!(started_at_from_session_id("nodash"), 0);
        assert_eq!(started_at_from_session_id("1234-notanumber"), 0);
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

    /// The manifest promises `history.sqlite` is excluded; this is the
    /// guard that makes the promise structural rather than a convention
    /// every caller has to remember.
    #[test]
    fn bundle_entries_refuses_a_history_database_from_any_source_list() {
        let entries = bundle_entries(
            &[
                PathBuf::from("/appdata/history.sqlite"),
                PathBuf::from("/logs/a.log"),
            ],
            &[PathBuf::from("/elsewhere/history.sqlite")],
            Some(Path::new("/appdata/history.sqlite")),
        );
        assert_eq!(
            entries,
            vec![("a.log".to_string(), PathBuf::from("/logs/a.log"))]
        );
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

    /// Shares `dump_format::ring_siblings` with the writer and the
    /// replay reader, so a hole in the numbering (a failed rotation, a
    /// hand-deleted chunk) doesn't orphan everything past it — the bundle
    /// hands over exactly the chunks those two still consider live.
    #[test]
    fn dump_ring_parts_still_collects_chunks_past_a_gap_in_the_numbering() {
        let path = temp_dump_path("gap");
        let dot2 = bpsr_protocol::dump_format::numbered_sibling(&path, 2);
        fs::write(&dot2, "oldest").unwrap();
        fs::write(&path, "newest").unwrap();

        let parts = dump_ring_parts(&path);

        assert_eq!(parts, vec![dot2.clone(), path.clone()]);

        let _ = fs::remove_file(&path);
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

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        let entries = vec![("session.log".to_string(), source.clone())];

        let missing = export_bundle_to(&dir, &entries, &manifest, None, true).unwrap();

        assert!(missing.is_empty());
        assert_eq!(fs::read(dir.join("session.log")).unwrap(), b"log contents");
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains("\"session_id\": \"1-1700000000\""));
        assert!(manifest_json.contains("history.sqlite"));
        assert!(manifest_json.contains("\"missing\": []"));

        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The TOCTOU the dump ring makes real: the writer thread can rotate a
    /// chunk away between `dump_ring_parts` listing it and `fs::copy`
    /// reaching it. The export still succeeds (a partial bundle beats no
    /// bundle), but says which file it came up short.
    #[test]
    fn export_bundle_to_reports_a_source_deleted_mid_export_in_the_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-toctou-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let kept = temp_dump_path("toctou-kept");
        let evicted = temp_dump_path("toctou-evicted");
        fs::write(&kept, b"live chunk").unwrap();
        fs::write(&evicted, b"about to be rotated away").unwrap();

        let entries = vec![
            ("dump.jsonl".to_string(), kept.clone()),
            ("dump.jsonl.1".to_string(), evicted.clone()),
        ];
        // Stands in for the writer thread rotating the chunk out from
        // under the export after `dump_ring_parts` listed it.
        fs::remove_file(&evicted).unwrap();

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: true,
                dump_byte_budget: 100,
                dropped_records: Some(0),
                dump_sanitized: true,
                sanitized_out_records: None,
            },
        );
        let missing = export_bundle_to(&dir, &entries, &manifest, None, true).unwrap();

        assert_eq!(missing, vec!["dump.jsonl.1".to_string()]);
        assert_eq!(fs::read(dir.join("dump.jsonl")).unwrap(), b"live chunk");
        assert!(!dir.join("dump.jsonl.1").exists());
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(
            manifest_json.contains("\"dump.jsonl.1\""),
            "manifest should name the file it couldn't copy: {manifest_json}"
        );

        let _ = fs::remove_file(&kept);
        let _ = fs::remove_dir_all(&dir);
    }

    /// End to end over the two functions the UI actually calls: a
    /// `history.sqlite` handed in as a log part never reaches the bundle
    /// directory, and the manifest still explains why it isn't there.
    #[test]
    fn export_never_copies_a_history_database_but_still_lists_it_as_excluded() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-history-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let history_dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-history-source-{}",
            std::process::id()
        ));
        fs::create_dir_all(&history_dir).unwrap();
        let history = history_dir.join(HISTORY_FILE_NAME);
        fs::write(&history, b"plaintext party member names").unwrap();
        let log = history_dir.join("session.log");
        fs::write(&log, b"log contents").unwrap();

        let entries = bundle_entries(&[history.clone(), log.clone()], &[], None);
        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        let missing = export_bundle_to(&dir, &entries, &manifest, None, true).unwrap();

        assert!(missing.is_empty());
        assert!(
            !dir.join(HISTORY_FILE_NAME).exists(),
            "the history database must never land in a bundle"
        );
        assert!(dir.join("session.log").exists());
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains(HISTORY_FILE_NAME));
        assert!(manifest_json.contains(HISTORY_EXCLUSION_REASON));

        let _ = fs::remove_dir_all(&history_dir);
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
        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: true,
                dump_byte_budget: 100,
                dropped_records: Some(0),
                dump_sanitized: true,
                sanitized_out_records: None,
            },
        );
        let entries = vec![("dump.jsonl".to_string(), missing)];

        let missing_names = export_bundle_to(&dir, &entries, &manifest, None, true).unwrap();

        assert_eq!(missing_names, vec!["dump.jsonl".to_string()]);
        assert!(!dir.join("dump.jsonl").exists());
        assert!(dir.join("manifest.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    // -- history sanitizing (issue #347) -----------------------------------

    fn seeded_history_db(tag: &str) -> PathBuf {
        use bpsr_meter::Class;

        use crate::history::sqlite::SqliteHistory;
        use crate::history::{EncounterRecord, HistoryStore, PlayerRecord, RetentionPolicy};

        let path = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-history-db-{tag}-{}.sqlite",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut store = SqliteHistory::open(&path, RetentionPolicy::default()).unwrap();
        store
            .insert(&EncounterRecord {
                ended_at_ms: 1_000,
                duration_ms: 10_000,
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
                players: vec![PlayerRecord {
                    uid: 1,
                    name: "Alice".to_string(),
                    class: Some(Class::FrostMage),
                    ability_score: Some(999),
                    season_strength: Some(42),
                    imagines: [None, None],
                    imagine_tiers: [None, None],
                    damage: 5_000,
                    dps: 500.0,
                    share_pct: 100.0,
                    crit_pct: 12.5,
                    lucky_pct: 6.25,
                    hits: 40,
                    deaths: 0,
                    skills: Vec::new(),
                }],
            })
            .unwrap();
        path
    }

    /// End to end over `export_bundle_to`'s history-sanitizing step: a real
    /// `history.sqlite` handed in as `history_source` lands in the bundle
    /// as `history.sanitized.sqlite` with its player name gone, the raw
    /// file itself never appears, and the manifest says sanitizing
    /// happened.
    #[test]
    fn export_bundle_to_includes_a_sanitized_history_copy_by_default() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-sanitize-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let history_db = seeded_history_db("include");

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        let missing = export_bundle_to(&dir, &[], &manifest, Some(&history_db), true).unwrap();

        assert!(missing.is_empty());
        assert!(!dir.join(HISTORY_FILE_NAME).exists());
        let sanitized_path = dir.join(SANITIZED_HISTORY_FILE_NAME);
        assert!(sanitized_path.exists());
        let contents = fs::read(&sanitized_path).unwrap();
        assert!(
            !contents.windows(5).any(|w| w == b"Alice"),
            "sanitized copy must not contain the real player name"
        );
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains("\"sanitized_history_included\": true"));

        let _ = fs::remove_file(&history_db);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_bundle_to_skips_history_when_include_history_is_false() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-sanitize-off-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let history_db = seeded_history_db("excluded");

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        export_bundle_to(&dir, &[], &manifest, Some(&history_db), false).unwrap();

        assert!(!dir.join(SANITIZED_HISTORY_FILE_NAME).exists());
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains("\"sanitized_history_included\": false"));

        let _ = fs::remove_file(&history_db);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A fresh install with no history database yet must not fail the
    /// whole export — sanitizing is simply skipped, same as any other
    /// best-effort entry that isn't there.
    #[test]
    fn export_bundle_to_tolerates_a_missing_history_database() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-sanitize-missing-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let missing_history = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-no-such-history-{}.sqlite",
            std::process::id()
        ));
        let _ = fs::remove_file(&missing_history);

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        let missing = export_bundle_to(&dir, &[], &manifest, Some(&missing_history), true).unwrap();

        assert!(missing.is_empty());
        assert!(!dir.join(SANITIZED_HISTORY_FILE_NAME).exists());
        assert!(dir.join("manifest.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `history_source` that exists but isn't a readable database
    /// (`sanitize_copy` fails) must not leave a sanitized file behind, and
    /// the manifest must say so — same contract as a missing source, just
    /// reached via a different `sanitize_copy` failure mode.
    #[test]
    fn export_bundle_to_leaves_out_the_sanitized_history_file_when_sanitizing_fails() {
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-export-sanitize-corrupt-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let corrupt_history = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-bundle-corrupt-history-{}.sqlite",
            std::process::id()
        ));
        fs::write(&corrupt_history, b"not a database").unwrap();

        let manifest = build_manifest(
            "1-1700000000",
            "0.2.6",
            1_700_000_000,
            DumpStatus {
                inspect_enabled: false,
                dump_byte_budget: 0,
                dropped_records: None,
                dump_sanitized: false,
                sanitized_out_records: None,
            },
        );
        let missing = export_bundle_to(&dir, &[], &manifest, Some(&corrupt_history), true).unwrap();

        assert!(missing.is_empty());
        assert!(!dir.join(SANITIZED_HISTORY_FILE_NAME).exists());
        let manifest_json = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_json.contains("\"sanitized_history_included\": false"));

        let _ = fs::remove_file(&corrupt_history);
        let _ = fs::remove_dir_all(&dir);
    }
}
