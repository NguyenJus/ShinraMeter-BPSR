//! Where the game-derived asset trees (`assets/classes/`, `assets/imagines/`)
//! live on disk.
//!
//! Three candidates are tried in order: an explicit `SHINRA_ASSETS_DIR`
//! override, `assets/` beside the running exe (the packaged-zip layout), and
//! `assets/` under the crate's manifest dir (the `cargo run` / `cargo test`
//! dev layout). `None` is not an error — the app runs iconless, degrading
//! the same way a decode failure does.

use std::path::{Path, PathBuf};

/// Resolves the asset root, trying `override_dir`, then `exe_dir.join("assets")`,
/// then `manifest_dir` (already the full `<...>/assets` path) in that order,
/// each candidate accepted only if it `is_dir()`. `warning` is `Some` only in
/// the final `None` case — matching `paths::resolve`'s "the fallback branch is
/// the one that warns".
pub fn resolve(
    override_dir: Option<&str>,
    exe_dir: Option<&Path>,
    manifest_dir: &str,
) -> (Option<PathBuf>, Option<String>) {
    if let Some(dir) = override_dir
        && !dir.is_empty()
        && Path::new(dir).is_dir()
    {
        return (Some(PathBuf::from(dir)), None);
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join("assets");
        if candidate.is_dir() {
            return (Some(candidate), None);
        }
    }
    let candidate = PathBuf::from(manifest_dir);
    if candidate.is_dir() {
        return (Some(candidate), None);
    }
    (
        None,
        Some(format!(
            "no asset root found (checked SHINRA_ASSETS_DIR, the exe's directory, and {manifest_dir}); \
             class and Imagine icons will not be shown"
        )),
    )
}

/// The impure wrapper: reads `SHINRA_ASSETS_DIR`, the running exe's
/// directory, and the crate's compiled-in manifest dir, then hands them to
/// [`resolve`].
pub fn root() -> (Option<PathBuf>, Option<String>) {
    resolve(
        std::env::var("SHINRA_ASSETS_DIR").ok().as_deref(),
        std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::parent),
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_override_wins() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let (dir, warning) = resolve(Some(manifest_dir), None, "/definitely/does/not/exist");
        assert_eq!(dir.as_deref(), Some(std::path::Path::new(manifest_dir)));
        assert!(warning.is_none());
    }

    #[test]
    fn an_empty_override_is_ignored() {
        let manifest_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
        let (dir, warning) = resolve(Some(""), None, manifest_dir);
        assert_eq!(dir.as_deref(), Some(std::path::Path::new(manifest_dir)));
        assert!(warning.is_none());
    }

    #[test]
    fn a_nonexistent_override_falls_through() {
        let manifest_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
        let (dir, warning) = resolve(Some("/definitely/does/not/exist"), None, manifest_dir);
        assert_eq!(dir.as_deref(), Some(std::path::Path::new(manifest_dir)));
        assert!(warning.is_none());
    }

    #[test]
    fn the_exe_dir_wins_over_the_manifest_dir() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let expected = std::path::PathBuf::from(manifest_dir).join("assets");
        // `manifest_dir` (`.../crates/app`) is a real, existing directory
        // that itself has an `assets/` subdir, so using it as the (fake) exe
        // dir proves candidate 2 is checked, and checked before a bogus
        // candidate 3.
        let (dir, warning) = resolve(
            None,
            Some(std::path::Path::new(manifest_dir)),
            "/definitely/does/not/exist",
        );
        assert_eq!(dir.as_deref(), Some(expected.as_path()));
        assert!(warning.is_none());
    }

    #[test]
    fn all_missing_yields_none_and_a_warning() {
        let (dir, warning) = resolve(
            None,
            Some(std::path::Path::new("/definitely/does/not/exist/exe")),
            "/definitely/does/not/exist/manifest",
        );
        assert!(dir.is_none());
        assert!(warning.is_some());
    }
}
