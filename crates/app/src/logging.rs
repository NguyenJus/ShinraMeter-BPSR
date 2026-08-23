//! Application-wide logging (issue #69).
//!
//! On by default — unlike a bare `env_logger::init()` (default filter
//! `error`-only), and unlike stderr alone, which goes nowhere: the binary
//! carries `#![cfg_attr(windows, windows_subsystem = "windows")]`, so a
//! shipped build has no console for stderr to land on. [`init`] installs a
//! logger at `info` by default (overridable with the standard `RUST_LOG` env
//! var) that writes to both stderr and a log file, so a user hitting a bug
//! can send us something.
//!
//! The log file defaults to
//! `%APPDATA%\ShinraMeter-BPSR\logs\ShinraMeter-BPSR.log` (or
//! `ShinraMeter-BPSR.log` in the working directory if `APPDATA` is unset —
//! e.g. this Linux dev host), overridable with `SHINRA_LOG_FILE=<path>`. It
//! is opened in append mode and rotated to `<path>.1` (replacing any
//! previous `.1`) whenever it grows past [`MAX_LOG_BYTES`] — checked both at
//! startup and, since an always-on overlay can run for days without one,
//! while the process is live (see [`Tee`]), so the log can't grow unbounded.
//!
//! Logs may contain player names and other identifying traffic — never
//! attach one to an issue or PR (see `.gitignore`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// A log file at or above this size gets rotated to `<path>.1`.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Installs the global logger (stderr + file, `info` by default, `RUST_LOG`
/// overridable) and a panic hook that logs uncaught panics before chaining
/// to whatever hook was previously installed. Logging still works to
/// stderr if the log file couldn't be opened — a failure to open it must
/// never abort startup. (The resolved log path is logged in the startup
/// banner below, so it isn't also returned here.)
pub fn init() {
    let (path, path_warning) = log_file_path();
    let mut startup_warnings = Vec::new();
    startup_warnings.extend(path_warning);

    if let Some(len) = fs::metadata(&path).ok().map(|meta| meta.len()) {
        rotate_if_needed(&path, len, &mut startup_warnings);
    }

    let file = match open_log_file(&path) {
        Ok(file) => Some(file),
        Err(err) => {
            startup_warnings.push(format!(
                "failed to open log file {} ({err}); logging to stderr only",
                path.display()
            ));
            None
        }
    };
    let resolved_path = file.is_some().then(|| path.clone());

    let env = env_logger::Env::default().default_filter_or("info");
    let mut builder = env_logger::Builder::from_env(env);
    match file {
        Some(file) => builder.target(env_logger::Target::Pipe(Box::new(Tee::new(path, file)))),
        None => builder.target(env_logger::Target::Stderr),
    };
    builder.init();

    // Deferred from above: the logger isn't live until `builder.init()`
    // returns, so any warning about the log file itself has to be replayed
    // through it afterward rather than logged inline as it's discovered.
    for warning in startup_warnings {
        log::warn!("{warning}");
    }

    install_panic_hook();

    log::info!(
        "{} v{} starting (pid {}, log file: {}, filter: {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        resolved_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none, stderr only>".to_string()),
        log::max_level(),
    );
}

/// Where the log file lives. See the module doc comment for the default and
/// the `SHINRA_LOG_FILE` override. `pub(crate)` (rather than private) since
/// `ui`'s "Export logs" header-menu item (issue #220) needs the resolved
/// path to know what to bundle up.
pub(crate) fn log_file_path() -> (PathBuf, Option<String>) {
    log_file_path_from(
        std::env::var("SHINRA_LOG_FILE").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
    )
}

/// Returns the resolved path plus, when the working-directory fallback was
/// used, a warning explaining why — the caller can't log it itself (the
/// logger doesn't exist yet at this point in `init`), so it's handed back
/// to be replayed through `log::warn!` once the logger is live.
fn log_file_path_from(log_file: Option<&str>, appdata: Option<&str>) -> (PathBuf, Option<String>) {
    crate::paths::resolve(
        log_file,
        appdata,
        &["ShinraMeter-BPSR", "logs", "ShinraMeter-BPSR.log"],
        "ShinraMeter-BPSR.log",
        "APPDATA is not set; falling back to a working-directory log file",
    )
}

/// True once a file has grown large enough to rotate. Shared with
/// `crate::dump`'s rotation of the packet-inspection dump file (issue #87),
/// so the two size-cap-and-rotate-to-`.1` behaviors stay identical.
pub(crate) fn should_rotate(len: u64, max_bytes: u64) -> bool {
    len >= max_bytes
}

/// The `<path>.1` a rotation moves `path` to. Shared with `crate::dump` —
/// see [`should_rotate`].
pub(crate) fn rotated_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    PathBuf::from(rotated)
}

/// Renames `path` to `<path>.1` (replacing any previous `.1`) if `len` (its
/// current size) is at or above [`MAX_LOG_BYTES`]. Best-effort: a rename
/// failure is pushed onto `warnings` rather than acted on — losing rotation
/// isn't worth aborting startup over.
fn rotate_if_needed(path: &Path, len: u64, warnings: &mut Vec<String>) {
    if !should_rotate(len, MAX_LOG_BYTES) {
        return;
    }
    let rotated = rotated_path(path);
    if let Err(err) = fs::rename(path, &rotated) {
        warnings.push(format!(
            "failed to rotate log file {} to {} ({err}); continuing without rotation",
            path.display(),
            rotated.display()
        ));
    }
}

/// Serializes [`Tee::rotate`]'s rename against [`export_logs_to`]'s read of
/// the parts it snapshotted (PR #227 review). Without it the logging thread
/// could rename `<path>` onto `<path>.1` in between — replacing the very
/// `.1` the export had already decided to bundle, and leaving a fresh empty
/// primary behind — so a whole rotation's worth of records would vanish
/// from the bundle with nothing reporting it.
///
/// Only the rename is guarded, not every record: an append racing an export
/// just adds lines to the tail of the part being copied, which is harmless.
///
/// [`Tee::rotate`] runs *inside* `env_logger`'s own writer lock, so whoever
/// holds this must never log while holding it — that would take the two
/// locks in the opposite order and deadlock. [`export_logs_to`] accordingly
/// logs nothing and only returns its errors, for the caller to report once
/// the guard is gone.
static ROTATION_LOCK: Mutex<()> = Mutex::new(());

/// Takes [`ROTATION_LOCK`], ignoring poisoning: the guarded sections are a
/// rename and a file copy, neither of which leaves shared state a panic
/// could have made inconsistent, and a poisoned lock must not be allowed to
/// wedge logging or exporting for the rest of the session.
fn lock_rotation() -> MutexGuard<'static, ()> {
    ROTATION_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Opens `path` for appending, creating its parent directories as needed.
fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Duplicates every write to both the log file and stderr, so a developer
/// running from a terminal sees the same records that land on disk. Handed
/// to `env_logger` as an `io::Write` via `Target::Pipe` — deliberately not
/// wrapped in a `BufWriter`, so each record hits disk as it's logged rather
/// than sitting in an in-process buffer.
///
/// Also owns runtime rotation: the startup check in [`init`] alone would let
/// an overlay that stays up for days grow one unbounded file, so the bytes
/// written are counted here and the file is rotated and reopened in place
/// the moment the running total crosses `max_bytes`. Counting rather than
/// re-`stat`ing keeps that off the per-record path; `env_logger` serializes
/// every call through its own lock, so the count needs no synchronization of
/// its own.
struct Tee {
    path: PathBuf,
    file: File,
    stderr: io::Stderr,
    /// Bytes in `file` right now — seeded from its length at open time (it
    /// is appended to, so it may already hold a previous session's records)
    /// and reset by each rotation.
    written: u64,
    max_bytes: u64,
}

impl Tee {
    fn new(path: PathBuf, file: File) -> Self {
        Self::with_max_bytes(path, file, MAX_LOG_BYTES)
    }

    fn with_max_bytes(path: PathBuf, file: File, max_bytes: u64) -> Self {
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Self {
            path,
            file,
            stderr: io::stderr(),
            written,
            max_bytes,
        }
    }

    /// Moves the current file to `<path>.1` and reopens `path` empty.
    ///
    /// Best-effort, like the startup rotation, but the failure can't be
    /// reported through `log::warn!`: this runs *inside* the logger's own
    /// writer, so logging here would re-enter `env_logger` while it holds
    /// the lock guarding this very `Tee`. The note goes straight down the
    /// two streams instead. The byte count is reset either way, so a
    /// persistently failing rotation is retried once per `max_bytes` rather
    /// than on every subsequent record.
    fn rotate(&mut self) {
        let rotated = rotated_path(&self.path);
        // Held across the rename (and the reopen that follows it) so an
        // "Export logs" bundle can't have snapshotted its parts before it
        // and read them back after it — see `ROTATION_LOCK`.
        let _guard = lock_rotation();
        // The handle stays open across the rename (Rust opens files with
        // `FILE_SHARE_DELETE` on Windows, so this is legal there too); it is
        // dropped by the reopen below, which is the last write it could have
        // taken anyway.
        let result = fs::rename(&self.path, &rotated)
            .and_then(|()| open_log_file(&self.path).map(|file| self.file = file));
        if let Err(err) = result {
            let note = format!(
                "[log rotation] failed to rotate {} to {} ({err}); continuing in place\n",
                self.path.display(),
                rotated.display()
            );
            let _ = self.file.write_all(note.as_bytes());
            let _ = self.stderr.write_all(note.as_bytes());
        }
        self.written = 0;
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // `write_all` rather than `write`: a short file write would
        // otherwise report fewer bytes written than `buf.len()`, and
        // `env_logger` would retry with the remainder — duplicating those
        // bytes to stderr below.
        self.file.write_all(buf)?;
        // Best-effort: stderr having nowhere to go (no console under
        // `windows_subsystem = "windows"`) must never break file logging.
        let _ = self.stderr.write_all(buf);
        self.written += buf.len() as u64;
        if should_rotate(self.written, self.max_bytes) {
            self.rotate();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let _ = self.stderr.flush();
        Ok(())
    }
}

/// Suggested file name the "Export logs" header-menu item (issue #220)
/// hands the native save dialog as a starting point — the user can still
/// rename it before saving, since the dialog is what lets them pick the
/// destination in the first place.
pub(crate) const EXPORT_DEFAULT_FILENAME: &str = "ShinraMeter-BPSR-logs.log";

/// Which on-disk log files a "Export logs" export (issue #220) should
/// bundle, oldest first: the rotated `<path>.1` (if [`Tee::rotate`] ever
/// ran this session or a previous one) followed by the live file at `path`.
/// Oldest-first so a plain concatenation reads chronologically, same
/// direction a user scrolling a single combined file would expect.
///
/// A part that doesn't exist on disk is simply left out rather than erroring
/// — there's nothing unusual about a fresh install with no rotation yet, or
/// (defensively, in tests) a primary file that hasn't been written to yet.
pub(crate) fn files_to_export(primary: &Path) -> Vec<PathBuf> {
    [rotated_path(primary), primary.to_path_buf()]
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// Bundles every file [`files_to_export`] finds for `primary` into a single
/// file at `dest` — the destination the user picked via the native save
/// dialog (`platform::choose_log_export_path`), since that dialog can only
/// ever choose one file, not a folder. Each part is preceded by a header
/// line naming its source path, so a multi-part export (current file plus a
/// rotated `.1`) still reads unambiguously once concatenated.
///
/// Errors if there is nothing to export ([`files_to_export`] came back
/// empty), if `dest` is itself one of those parts (see below), or if any
/// part can't be read, or `dest` can't be written — a
/// half-written export file is still useful information (see below), so a
/// failure partway through is reported rather than cleaned up: the caller
/// only logs a warning on `Err` (issue #220 is a best-effort debugging aid,
/// not a critical path), and a partial file on disk showing exactly where
/// the read/write failed is more useful to whoever's debugging the bug
/// report than silently deleting it.
pub(crate) fn export_logs_to(primary: &Path, dest: &Path) -> io::Result<()> {
    // Held for the whole export, the snapshot below included, so a
    // concurrent `Tee::rotate` can't rename a part out from under the copy.
    // Nothing in here may log while it is held — see `ROTATION_LOCK`.
    let _guard = lock_rotation();

    let parts = files_to_export(primary);
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no log file found at {} to export", primary.display()),
        ));
    }

    // Exporting *onto* one of the sources would gut the log being exported:
    // the `File::create` below truncates `dest` before any part is opened,
    // so the "export" would read back empty (and the live log with it). The
    // save dialog can't catch this — `OFN_OVERWRITEPROMPT` happily accepts
    // the app's own log file, and asking the user to confirm an overwrite
    // is not the same as telling them it destroys the thing they're trying
    // to hand over — so the destination is checked here instead.
    let dest_key = comparable_path(dest)?;
    if let Some(part) = parts
        .iter()
        .find(|part| comparable_path(part).is_ok_and(|key| key == dest_key))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is one of the log files being exported; pick a different destination",
                part.display()
            ),
        ));
    }

    let mut out = File::create(dest)?;
    for part in parts {
        writeln!(out, "----- {} -----", part.display())?;
        let mut source = File::open(&part)?;
        io::copy(&mut source, &mut out)?;
        writeln!(out)?;
    }
    Ok(())
}

/// The form of `path` that any two paths naming the same file share —
/// symlinks, `.` and `..` resolved — so a destination spelled
/// `logs\..\logs\ShinraMeter-BPSR.log` is still recognized as the live log
/// (PR #227 review).
///
/// `canonicalize` only works on a file that exists, and an export
/// destination usually doesn't yet — naming a new file is the save
/// dialog's whole job — so a missing path falls back to canonicalizing its
/// parent directory (which the dialog's `OFN_PATHMUSTEXIST` guarantees does
/// exist) and re-joining the file name. That is enough for the comparison
/// it feeds: a destination that doesn't exist can't be a source part, which
/// by definition does.
fn comparable_path(path: &Path) -> io::Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} names no file to export to", path.display()),
        )
    })?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        // A bare file name is relative to the working directory — the same
        // place `log_file_path_from`'s own fallback puts the log file.
        _ => Path::new("."),
    };
    Ok(parent.canonicalize()?.join(name))
}

/// Chains onto whatever panic hook was previously installed (never replaces
/// it silently) and additionally logs the panic's payload and location at
/// `error` — with no console under `windows_subsystem = "windows"`, an
/// unlogged panic is otherwise completely invisible.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<no message>");
        log::error!("panic at {location}: {message}");
        // Issue #119: a `Surface::configure` panic can follow an oversize
        // window proposal even after `platform::window_proc` clamped it —
        // the clamp is not the only path to an oversize surface — so carry
        // whatever the platform layer last saw onto the same log line the
        // panic itself reaches. `last_oversize_proposal` is poison-safe and
        // never panics, so it's fine to call from inside a panic hook.
        //
        // The record is never cleared, so it can be arbitrarily stale by
        // the time a panic reads it — a clamp from early in a multi-day
        // session is not "before this panic" in any causal sense once an
        // unrelated crash happens days later. `last_oversize_proposal`
        // already prefixes its own age (e.g. "3d 4h ago"), so this line
        // only reports what was seen, not when relative to the panic.
        if let Some(proposal) = crate::platform::last_oversize_proposal() {
            log::error!("{proposal}");
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use bpsr_test_support::scratch_path;

    use super::*;

    // -- log_file_path ----------------------------------------------------

    #[test]
    fn log_file_path_prefers_the_explicit_override() {
        let (path, warning) = log_file_path_from(Some("/tmp/custom.log"), Some("/appdata"));
        assert_eq!(path, PathBuf::from("/tmp/custom.log"));
        assert!(warning.is_none());
    }

    #[test]
    fn log_file_path_falls_back_to_appdata_when_unset() {
        let (path, warning) = log_file_path_from(None, Some("/appdata"));
        assert_eq!(
            path,
            PathBuf::from("/appdata/ShinraMeter-BPSR/logs/ShinraMeter-BPSR.log")
        );
        assert!(warning.is_none());
    }

    #[test]
    fn log_file_path_falls_back_to_working_directory_when_neither_is_set() {
        let (path, warning) = log_file_path_from(None, None);
        assert_eq!(path, PathBuf::from("ShinraMeter-BPSR.log"));
        assert!(warning.is_some());

        let (path, warning) = log_file_path_from(Some(""), Some(""));
        assert_eq!(path, PathBuf::from("ShinraMeter-BPSR.log"));
        assert!(warning.is_some());
    }

    // -- should_rotate ------------------------------------------------------

    #[test]
    fn should_rotate_is_false_below_the_threshold() {
        assert!(!should_rotate(MAX_LOG_BYTES - 1, MAX_LOG_BYTES));
        assert!(!should_rotate(0, MAX_LOG_BYTES));
    }

    #[test]
    fn should_rotate_is_true_at_or_above_the_threshold() {
        assert!(should_rotate(MAX_LOG_BYTES, MAX_LOG_BYTES));
        assert!(should_rotate(MAX_LOG_BYTES + 1, MAX_LOG_BYTES));
    }

    // -- Tee ----------------------------------------------------------------

    /// The threshold is crossed by a running process, not just found crossed
    /// at startup — an always-on overlay never restarts, so a startup-only
    /// check would let its log grow forever.
    #[test]
    fn tee_rotates_when_the_running_total_crosses_the_threshold() {
        let path = scratch_path("log-rotate");
        let rotated = rotated_path(&path);
        let mut tee = Tee::with_max_bytes(path.clone(), open_log_file(&path).unwrap(), 8);

        tee.write_all(b"1234").unwrap();
        assert!(!rotated.exists());

        tee.write_all(b"5678").unwrap();
        assert_eq!(fs::read(&rotated).unwrap(), b"12345678");

        // The reopened file, not the rotated-away one, takes what follows.
        tee.write_all(b"9").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"9");
        assert_eq!(fs::read(&rotated).unwrap(), b"12345678");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    /// The file is opened in append mode, so a previous session's records
    /// count toward the threshold too.
    #[test]
    fn tee_seeds_its_count_from_the_file_it_appends_to() {
        let path = scratch_path("log-seed");
        let rotated = rotated_path(&path);
        fs::write(&path, b"previous session").unwrap();
        let mut tee = Tee::with_max_bytes(path.clone(), open_log_file(&path).unwrap(), 20);

        tee.write_all(b"more").unwrap();
        assert_eq!(fs::read(&rotated).unwrap(), b"previous sessionmore");
        assert_eq!(fs::read(&path).unwrap(), b"");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    // -- files_to_export / export_logs_to (issue #220) ----------------------

    #[test]
    fn files_to_export_returns_only_the_primary_when_nothing_rotated() {
        let path = scratch_path("export-primary-only");
        fs::write(&path, b"session log").unwrap();

        assert_eq!(files_to_export(&path), vec![path.clone()]);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn files_to_export_puts_the_rotated_file_before_the_primary() {
        let path = scratch_path("export-with-rotation");
        let rotated = rotated_path(&path);
        fs::write(&rotated, b"older session").unwrap();
        fs::write(&path, b"current session").unwrap();

        assert_eq!(files_to_export(&path), vec![rotated.clone(), path.clone()]);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    #[test]
    fn files_to_export_is_empty_when_neither_file_exists_yet() {
        let path = scratch_path("export-neither-exists");
        // Deliberately not created — e.g. exported before the app has
        // logged anything this session.
        assert!(files_to_export(&path).is_empty());
    }

    #[test]
    fn export_logs_to_copies_a_single_file_verbatim_under_a_header() {
        let path = scratch_path("export-dest-single-source");
        let dest = scratch_path("export-dest-single-dest");
        fs::write(&path, b"line one\nline two\n").unwrap();

        export_logs_to(&path, &dest).unwrap();

        let exported = fs::read_to_string(&dest).unwrap();
        assert!(exported.contains(&path.display().to_string()));
        assert!(exported.contains("line one\nline two\n"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&dest);
    }

    #[test]
    fn export_logs_to_orders_the_rotated_part_before_the_current_one() {
        let path = scratch_path("export-dest-multi-source");
        let rotated = rotated_path(&path);
        let dest = scratch_path("export-dest-multi-dest");
        fs::write(&rotated, b"OLDER-PART").unwrap();
        fs::write(&path, b"NEWER-PART").unwrap();

        export_logs_to(&path, &dest).unwrap();

        let exported = fs::read_to_string(&dest).unwrap();
        assert!(
            exported.find("OLDER-PART").unwrap() < exported.find("NEWER-PART").unwrap(),
            "rotated (older) content must come before the current file's content: {exported:?}"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
        let _ = fs::remove_file(&dest);
    }

    #[test]
    fn export_logs_to_errors_when_there_is_nothing_to_export() {
        let path = scratch_path("export-dest-nothing-source");
        let dest = scratch_path("export-dest-nothing-dest");
        // Neither `path` nor its rotated sibling exists.

        assert!(export_logs_to(&path, &dest).is_err());
    }

    /// The destination is truncated before any source is opened, so an
    /// export onto the live log (or its rotated sibling) would destroy the
    /// very records it was meant to hand over. Both parts must be rejected
    /// *and* left untouched.
    #[test]
    fn export_logs_to_refuses_a_destination_that_is_one_of_its_sources() {
        let path = scratch_path("export-dest-is-a-source");
        let rotated = rotated_path(&path);
        fs::write(&rotated, b"OLDER-PART").unwrap();
        fs::write(&path, b"NEWER-PART").unwrap();

        let err = export_logs_to(&path, &path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = export_logs_to(&path, &rotated).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // Spelled differently, same file — the comparison canonicalizes, so
        // a detour through the parent directory doesn't slip past it.
        let indirect = path
            .parent()
            .unwrap()
            .join(".")
            .join(path.file_name().unwrap());
        let err = export_logs_to(&path, &indirect).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        assert_eq!(fs::read(&path).unwrap(), b"NEWER-PART");
        assert_eq!(fs::read(&rotated).unwrap(), b"OLDER-PART");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    #[test]
    fn default_export_filename_is_a_stable_shinra_named_log_file() {
        assert_eq!(EXPORT_DEFAULT_FILENAME, "ShinraMeter-BPSR-logs.log");
    }
}
