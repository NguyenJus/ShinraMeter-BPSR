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
//!
//! One case is not a second instance at all and must not be refused: the
//! in-place updater (issue #250) starts the freshly-swapped executable and
//! does *not* wait for it, so the replacement runs `acquire` while the
//! process that spawned it is still draining capture, the pipeline, history
//! and settings on its way out — and still holding this lock, because the
//! kernel only drops it when that process actually ends. The replacement
//! would lose that race far more often than not and exit, which the user
//! sees as "I clicked update and the app just closed". `HANDOFF_VAR` is how
//! the outgoing process says so: it waits for the slot instead of refusing
//! it. Nothing else sets that variable, so a genuine second copy is still
//! turned away on its first try.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Set by [`crate::update_check::relaunch`] on the process it spawns, marking
/// it as the successor of an instance that is already on its way out. Any
/// non-empty value other than `0` means "wait for the slot"; see the module
/// doc comment for why the wait is needed at all.
pub const HANDOFF_VAR: &str = "SHINRA_INSTANCE_HANDOFF";

/// How long a relaunched instance waits for its predecessor to let go.
///
/// Generous on purpose: the outgoing process still has a window to tear
/// down, a WinDivert handle to close and three threads to join (the history
/// one flushes the session's last encounter to SQLite) before the kernel
/// releases its handle. Bounded all the same — if the predecessor is wedged
/// rather than exiting, the successor eventually says so instead of hanging
/// with no window and no message.
///
/// Kept at 10s rather than shortened (issue #278 review): the joins on the
/// outgoing side (`pipeline_thread`, the names-cache writer, the settings
/// thread — see `main.rs`'s shutdown path) block on `JoinHandle::join` with
/// no internal timeout of their own, so nothing bounds how long a slow but
/// healthy shutdown — e.g. the SQLite flush contending with disk I/O — can
/// take. A shorter ceiling would risk cutting off exactly the non-wedged
/// case this wait exists to cover; there was no evidence here that 5s is
/// still safe.
const HANDOFF_WAIT: Duration = Duration::from_secs(10);

/// How often the wait re-tries. Short enough that a normal handoff — a
/// couple of hundred milliseconds — costs the user no visible delay.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// How long [`acquire`] should wait for the slot, given `HANDOFF_VAR`'s
/// value. Takes the value rather than reading the environment so it stays
/// testable, the same shape [`lock_file_path`]'s inputs take.
fn handoff_wait(flag: Option<&str>) -> Duration {
    match flag {
        Some(value) if !value.is_empty() && value != "0" => HANDOFF_WAIT,
        _ => Duration::ZERO,
    }
}

/// Reads [`HANDOFF_VAR`] and removes it from this process's own
/// environment in the same step, so the flag is consumed exactly once
/// rather than staying live for the rest of the process's life.
///
/// # Safety
///
/// `std::env::remove_var` is `unsafe` on the 2024 edition because mutating
/// the environment races with any other thread that reads or writes it
/// concurrently. This is called from [`acquire`], which runs at the very
/// top of `main` — right after `logging::init` (which only *reads* env
/// vars such as `RUST_LOG`/`APPDATA`/`SHINRA_LOG_FILE` and spawns nothing)
/// and before this process spawns any thread of its own. The app crate's
/// other `std::thread::spawn`/`Builder` call sites are all in `settings`,
/// `pipeline`, `dump` and `ui`, none of which run before this point, and
/// the crate has no `ctor`-style static initializer either. No other
/// thread exists yet to race with, so the mutation is sound here — it
/// would not be, called anywhere past that point.
fn consume_handoff_var() -> Option<String> {
    let value = std::env::var(HANDOFF_VAR).ok();
    // SAFETY: see the function doc comment above.
    unsafe {
        std::env::remove_var(HANDOFF_VAR);
    }
    value
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
    let wait = handoff_wait(consume_handoff_var().as_deref());
    let acquisition = if wait.is_zero() {
        acquire_at_within(&path, wait)
    } else {
        acquire_with_logged_handoff(&path, wait)
    };
    if matches!(acquisition, Acquisition::Acquired(_)) {
        log::info!("single-instance lock acquired at {}", path.display());
    }
    acquisition
}

/// [`acquire_at_within`], but only for the relaunch case, and narrating the
/// wait to the log as it happens (issue #278 review): `acquire` runs before
/// any window, splash or tray icon exists, so if the predecessor is wedged
/// rather than exiting, a silent wait here is the only place the delay
/// could ever be explained. The "waiting" line fires only once the first
/// attempt actually finds the slot held — not merely because a wait is
/// armed — so a handoff that wins immediately logs nothing extra.
fn acquire_with_logged_handoff(path: &Path, wait: Duration) -> Acquisition {
    match acquire_at(path) {
        Acquisition::AlreadyRunning => {}
        settled => return settled,
    }
    log::info!(
        "relaunched by an in-place update; the previous instance still holds {} — waiting up to {}s for it to release the slot",
        path.display(),
        wait.as_secs()
    );
    let started = Instant::now();
    let outcome = acquire_at_within(path, wait);
    match &outcome {
        Acquisition::Acquired(_) => log::info!(
            "previous instance released {} after {:.2}s",
            path.display(),
            started.elapsed().as_secs_f64()
        ),
        Acquisition::AlreadyRunning => log::warn!(
            "gave up waiting for the previous instance to release {} after {}s; it may be stuck rather than exiting",
            path.display(),
            wait.as_secs()
        ),
        Acquisition::Unavailable(_) => {}
    }
    outcome
}

/// [`acquire_at`], but willing to wait up to `wait` for a lock another
/// process is still holding, instead of refusing on the first try.
///
/// Only `AlreadyRunning` is retried: [`Acquisition::Unavailable`] means the
/// lock could not be evaluated at all, and re-evaluating it every 25ms for
/// ten seconds would not change that. A zero `wait` is exactly one attempt,
/// which is what every caller but the relaunched successor gets — the guard
/// is no more permissive than it was, it just no longer refuses a slot that
/// is in the middle of being handed to it.
pub fn acquire_at_within(path: &Path, wait: Duration) -> Acquisition {
    let deadline = Instant::now() + wait;
    loop {
        match acquire_at(path) {
            Acquisition::AlreadyRunning => {}
            settled => return settled,
        }
        let now = Instant::now();
        if now >= deadline {
            return Acquisition::AlreadyRunning;
        }
        std::thread::sleep(RETRY_INTERVAL.min(deadline - now));
    }
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

    /// The in-place updater's successor must not be turned away by the
    /// predecessor that spawned it: the outgoing process holds the lock
    /// until the kernel closes its handle, which is after this one has
    /// already started and asked.
    #[test]
    fn a_relaunched_instance_waits_for_its_predecessor_to_let_go() {
        let path = lock_path("handoff");
        let predecessor = acquire_at(&path);
        assert!(matches!(predecessor, Acquisition::Acquired(_)));

        // Stands in for the outgoing process finishing its shutdown: the
        // lock is only released well after the successor starts asking.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(predecessor);
        });

        let started = Instant::now();
        let successor = acquire_at_within(&path, Duration::from_secs(10));
        assert!(
            matches!(successor, Acquisition::Acquired(_)),
            "a relaunched instance must wait for the slot, not exit"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "it should have actually waited rather than won on the first try"
        );
        releaser.join().unwrap();
    }

    /// The wait is a delay, not an amnesty: a genuine second copy that never
    /// gets the slot is still refused once the window closes.
    #[test]
    fn a_second_instance_is_still_refused_once_the_wait_runs_out() {
        let path = lock_path("handoff-expired");
        let first = acquire_at(&path);
        assert!(matches!(first, Acquisition::Acquired(_)));

        let started = Instant::now();
        assert!(
            matches!(
                acquire_at_within(&path, Duration::from_millis(150)),
                Acquisition::AlreadyRunning
            ),
            "waiting must not turn into letting a real second instance run"
        );
        assert!(started.elapsed() >= Duration::from_millis(150));

        drop(first);
    }

    /// The ordinary launch path is unchanged: one attempt, no delay before
    /// the user is told the meter is already running.
    #[test]
    fn a_zero_wait_refuses_immediately() {
        let path = lock_path("handoff-none");
        let first = acquire_at(&path);
        assert!(matches!(first, Acquisition::Acquired(_)));

        let started = Instant::now();
        assert!(matches!(
            acquire_at_within(&path, Duration::ZERO),
            Acquisition::AlreadyRunning
        ));
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(first);
    }

    /// An unusable lock path is a verdict, not a race — retrying it for ten
    /// seconds would only delay a startup that is going to continue anyway.
    #[test]
    fn an_unusable_lock_path_is_not_retried_even_with_a_wait() {
        let blocker = lock_path("handoff-blocked");
        assert!(matches!(acquire_at(&blocker), Acquisition::Acquired(_)));
        let path = blocker.join("instance.lock");

        let started = Instant::now();
        assert!(matches!(
            acquire_at_within(&path, Duration::from_secs(10)),
            Acquisition::Unavailable(_)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn only_the_relaunch_flag_asks_for_a_wait() {
        assert_eq!(handoff_wait(None), Duration::ZERO);
        assert_eq!(handoff_wait(Some("")), Duration::ZERO);
        assert_eq!(handoff_wait(Some("0")), Duration::ZERO);
        assert_eq!(handoff_wait(Some("1")), HANDOFF_WAIT);
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
