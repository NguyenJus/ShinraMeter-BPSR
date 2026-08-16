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
/// the `SHINRA_LOG_FILE` override.
fn log_file_path() -> (PathBuf, Option<String>) {
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

/// True once a log file has grown large enough to rotate.
fn should_rotate(len: u64, max_bytes: u64) -> bool {
    len >= max_bytes
}

/// The `<path>.1` a rotation moves `path` to.
fn rotated_path(path: &Path) -> PathBuf {
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
}
