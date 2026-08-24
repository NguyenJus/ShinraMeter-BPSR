//! One meter process per machine (issue #277).
//!
//! The overlay minimizes to the notification area rather than exiting, so
//! "I closed it" and "it is still running" look identical to the user — and
//! launching a second copy is easy to do by accident. Two live copies then
//! share the two files the app appends to: they interleave into one log
//! (every event line appears twice, from two independent capture loops) and
//! they both write the same fight to `history.sqlite`, which is where it
//! stops being cosmetic — the history list grows a duplicate row per fight.
//!
//! Note what does *not* happen: WinDivert sniff handles each get their own
//! copy of the packet stream, so neither instance sees a packet twice.
//! Damage and hit counts inside an encounter stay correct; it is the *rows*
//! that duplicate, not the numbers in them.
//!
//! The guard is an advisory lock on a file next to the log and the database
//! ([`lock_file_path`]), held for the life of the process. The OS drops the
//! lock when the process ends however it ends, so a crashed instance never
//! leaves a stale lock behind that would lock the user out of their own
//! meter — which is why this is a file lock rather than a pid file.
//!
//! `SHINRA_INSTANCE_LOCK` overrides the path, which is also the escape hatch
//! for deliberately running two builds side by side: point them at different
//! lock files (and different `SHINRA_HISTORY_DB`s) and both start.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::paths;

/// A held single-instance lock. Keep it alive for as long as the process
/// should hold the meter slot — dropping it (or exiting) releases the lock.
#[derive(Debug)]
pub struct InstanceGuard {
    /// The lock lives on the open handle, not on the file's contents: the
    /// field is never read, only kept from being dropped.
    _file: File,
}

/// What [`acquire_at`] found.
#[derive(Debug)]
pub enum Acquisition {
    /// This process now owns the meter slot.
    Acquired(InstanceGuard),
    /// Another live instance already owns it; this one must not start.
    AlreadyRunning,
    /// The lock could not be evaluated at all (unwritable directory, a
    /// filesystem without locking). Startup continues without a guard —
    /// refusing to run because the *guard* is broken would be a worse bug
    /// than the one it prevents — and the caller logs `reason`.
    Unavailable(String),
}

/// Where the lock file lives: `%APPDATA%\ShinraMeter-BPSR\instance.lock`,
/// overridden outright by `SHINRA_INSTANCE_LOCK`, falling back to a
/// working-directory file (with a warning) when `APPDATA` is unset — the
/// same resolution every other app file gets, see [`crate::paths`].
pub fn lock_file_path() -> (PathBuf, Option<String>) {
    paths::resolve(
        std::env::var("SHINRA_INSTANCE_LOCK").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
        &["ShinraMeter-BPSR", "instance.lock"],
        "ShinraMeter-BPSR-instance.lock",
        "APPDATA is not set; falling back to a working-directory file for the single-instance lock",
    )
}

/// Resolves the lock path and tries to claim the meter slot, logging the
/// path warning (if any) on the way through, plus a debug breadcrumb on
/// success — otherwise a claimed lock leaves no trace in the log at all,
/// and a `SHINRA_INSTANCE_LOCK` override would be invisible to a support
/// log. Call once, early in `main`, and hold the returned guard for the
/// life of the process.
pub fn acquire() -> Acquisition {
    let (path, warning) = lock_file_path();
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }
    let acquisition = acquire_at(&path);
    if matches!(acquisition, Acquisition::Acquired(_)) {
        log::info!("single-instance lock acquired at {}", path.display());
    }
    acquisition
}

/// [`acquire`] against an explicit path — the testable half.
///
/// The lock is taken on a handle that stays open inside the returned guard.
/// The pid written into the file is diagnostic only: nothing reads it back,
/// because a pid on disk cannot distinguish "still running" from "crashed,
/// and the number has since been reused".
pub fn acquire_at(path: &Path) -> Acquisition {
    if let Err(err) = paths::ensure_parent_dir(path) {
        let parent = path.parent().unwrap_or(path);
        return Acquisition::Unavailable(format!(
            "could not create {} for the single-instance lock ({err})",
            parent.display()
        ));
    }

    // Not `truncate(true)`: truncation happens *after* the lock is won, so a
    // losing instance never touches the winner's file contents.
    let mut file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(err) => {
            return Acquisition::Unavailable(format!(
                "could not open the single-instance lock {} ({err})",
                path.display()
            ));
        }
    };

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Acquisition::AlreadyRunning,
        Err(TryLockError::Error(err)) => {
            return Acquisition::Unavailable(format!("could not lock {} ({err})", path.display()));
        }
    }

    // Best-effort breadcrumb for whoever reads the directory later; a failure
    // here does not invalidate the lock we already hold.
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();

    Acquisition::Acquired(InstanceGuard { _file: file })
}

/// What the refused instance says before it exits — one line, naming the
/// notification area, because that is where the instance it lost to almost
/// certainly is.
pub const ALREADY_RUNNING_MESSAGE: &str = "ShinraMeter-BPSR is already running; look for it in the notification area. \
     A second copy would write every fight to the history twice, so this one is exiting (issue #277).";

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Each test gets its own directory, so a lock left held by one cannot
    /// change what another observes.
    fn lock_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shinra-single-instance-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir.join("instance.lock")
    }

    #[test]
    fn the_first_instance_acquires_the_lock() {
        let path = lock_path("first");
        assert!(matches!(acquire_at(&path), Acquisition::Acquired(_)));
        assert!(path.exists(), "the lock file should have been created");
    }

    /// The whole point: with one instance holding the lock, the next one is
    /// told to stand down rather than quietly duplicating its work.
    #[test]
    fn a_second_instance_is_refused_while_the_first_holds_the_lock() {
        let path = lock_path("second");
        let first = acquire_at(&path);
        assert!(matches!(first, Acquisition::Acquired(_)));

        assert!(
            matches!(acquire_at(&path), Acquisition::AlreadyRunning),
            "a second acquisition must not succeed while the first guard is alive"
        );

        drop(first);
    }

    /// A refusal must not outlive the instance that caused it — otherwise a
    /// crash would lock the user out of their own meter.
    #[test]
    fn dropping_the_guard_frees_the_slot_for_the_next_instance() {
        let path = lock_path("released");
        let first = acquire_at(&path);
        assert!(matches!(first, Acquisition::Acquired(_)));
        drop(first);

        assert!(
            matches!(acquire_at(&path), Acquisition::Acquired(_)),
            "the slot must be reclaimable once the holder is gone"
        );
    }

    /// The guard creates its own directory: on a first-ever launch nothing
    /// under `%APPDATA%` exists yet, and failing there would refuse every
    /// instance including the first.
    #[test]
    fn a_missing_parent_directory_is_created_rather_than_failing() {
        let path = lock_path("nested").join("deeper").join("instance.lock");
        assert!(matches!(acquire_at(&path), Acquisition::Acquired(_)));
    }

    /// A lock that cannot be evaluated must not take the app down with it.
    #[test]
    fn an_unusable_lock_path_reports_unavailable_rather_than_refusing() {
        // A regular file makes a hopeless parent directory.
        let blocker = lock_path("blocked");
        assert!(matches!(acquire_at(&blocker), Acquisition::Acquired(_)));
        let path = blocker.join("instance.lock");

        match acquire_at(&path) {
            Acquisition::Unavailable(reason) => assert!(!reason.is_empty()),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_lock_file_records_the_holders_pid() {
        let path = lock_path("pid");
        let guard = acquire_at(&path);
        assert!(matches!(guard, Acquisition::Acquired(_)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        drop(guard);
    }
}
