//! Unpacking the embedded WinDivert runtime to disk.
//!
//! Windows loads kernel drivers through the Service Control Manager, which
//! takes a *filesystem path* — so `WinDivert64.sys` has to exist as a real
//! file no matter how the executable is packaged. This module owns that
//! write, and is deliberately platform-neutral so its decisions (where the
//! files go, when a rewrite is actually needed) are host-testable; the
//! Windows-only FFI that consumes them lives in [`crate::driver`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the runtime is unpacked, under a per-user writable root.
///
/// Versioned so upgrading the bundled WinDivert never collides with a copy an
/// older build left behind — and so a driver still resident from a previous
/// version is not silently overwritten underneath the kernel.
pub fn runtime_dir(base: &Path, version: &str) -> PathBuf {
    base.join("ShinraMeter-BPSR")
        .join("windivert")
        .join(version)
}

/// Writes `want` to `path` unless the file already holds exactly those bytes.
/// Returns whether a write actually happened.
///
/// The content check is what makes startup cheap and quiet: on every run
/// after the first there is nothing to write, so the executable does not
/// re-drop a kernel driver each launch (which is both wasteful and the exact
/// behaviour antivirus heuristics watch for).
///
/// The write goes to a sibling temporary file and is renamed into place, so a
/// crash or a full disk can never leave a half-written driver where the next
/// run would load it.
pub fn ensure_file(path: &Path, want: &[u8]) -> io::Result<bool> {
    if fs::read(path).is_ok_and(|have| have == want) {
        return Ok(false);
    }

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;

    let staging = staging_path(path);
    fs::write(&staging, want)?;
    match fs::rename(&staging, path) {
        Ok(()) => Ok(true),
        Err(err) => {
            // Leaving the staging file behind would accumulate one dead copy
            // per failed launch.
            let _ = fs::remove_file(&staging);
            Err(err)
        }
    }
}

/// Sibling path the new bytes are staged at before being renamed into place.
fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch directory per test, without a `tempfile` dependency.
    fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bpsr-install-{}-{}-{}", std::process::id(), tag, n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn runtime_dir_is_versioned_under_the_base() {
        let dir = runtime_dir(Path::new("/base"), "2.2.2");
        assert!(dir.ends_with("ShinraMeter-BPSR/windivert/2.2.2"));
    }

    #[test]
    fn runtime_dirs_for_different_versions_do_not_collide() {
        let base = Path::new("/base");
        assert_ne!(runtime_dir(base, "2.2.2"), runtime_dir(base, "2.2.3"));
    }

    #[test]
    fn ensure_file_writes_when_missing() {
        let dir = scratch("missing");
        let path = dir.join("nested").join("WinDivert64.sys");

        assert!(ensure_file(&path, b"driver").expect("write"));
        assert_eq!(fs::read(&path).expect("read back"), b"driver");
    }

    #[test]
    fn ensure_file_skips_when_contents_already_match() {
        let dir = scratch("match");
        let path = dir.join("WinDivert.dll");
        ensure_file(&path, b"library").expect("first write");

        assert!(
            !ensure_file(&path, b"library").expect("second call"),
            "identical contents must not be rewritten"
        );
    }

    #[test]
    fn ensure_file_rewrites_when_contents_differ() {
        let dir = scratch("differ");
        let path = dir.join("WinDivert.dll");
        ensure_file(&path, b"old version").expect("first write");

        assert!(ensure_file(&path, b"new version").expect("rewrite"));
        assert_eq!(fs::read(&path).expect("read back"), b"new version");
    }

    #[test]
    fn ensure_file_leaves_no_staging_file_behind() {
        let dir = scratch("staging");
        let path = dir.join("WinDivert.dll");
        ensure_file(&path, b"library").expect("write");

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().ends_with(".new"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");
    }
}
