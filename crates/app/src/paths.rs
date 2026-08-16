//! Where the app's on-disk files live.
//!
//! Every one of them — the log file ([`crate::logging`]), the inspect dump
//! ([`crate::inspect`]) and the name cache (`main`) — resolves the same way:
//! an explicit env-var override wins, otherwise the file hangs under
//! `%APPDATA%\ShinraMeter-BPSR\`, otherwise (no `APPDATA` — e.g. this Linux
//! dev host, CI) it lands in the working directory and the caller says so.
//! That shape lives here once rather than in three near-identical copies.

use std::path::PathBuf;

/// Resolves one such path.
///
/// `override_path` is the caller's env-var value (`None` for a file with no
/// override), `appdata` is `%APPDATA%`, `appdata_subpath` the components to
/// hang under it, and `fallback` the working-directory file name used when
/// `APPDATA` is unset — the case that also hands `warning` back.
///
/// The warning is returned rather than logged here because the callers have
/// different sinks for it: `inspect` and `main` resolve their paths with the
/// logger already live and can `log::warn!` on the spot, while `logging`
/// resolves its own path *before* the logger exists and has to defer its
/// warnings until after `env_logger` is installed (see `logging::init`).
pub fn resolve(
    override_path: Option<&str>,
    appdata: Option<&str>,
    appdata_subpath: &[&str],
    fallback: &str,
    warning: &str,
) -> (PathBuf, Option<String>) {
    if let Some(path) = override_path
        && !path.is_empty()
    {
        return (PathBuf::from(path), None);
    }
    match appdata {
        Some(appdata) if !appdata.is_empty() => (
            appdata_subpath
                .iter()
                .fold(PathBuf::from(appdata), |path, part| path.join(part)),
            None,
        ),
        _ => (PathBuf::from(fallback), Some(warning.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBPATH: &[&str] = &["ShinraMeter-BPSR", "logs", "app.log"];

    #[test]
    fn an_override_wins_over_appdata() {
        let (path, warning) = resolve(Some("/tmp/custom"), Some("/appdata"), SUBPATH, "fb", "w");
        assert_eq!(path, PathBuf::from("/tmp/custom"));
        assert!(warning.is_none());
    }

    #[test]
    fn an_empty_override_is_ignored() {
        let (path, warning) = resolve(Some(""), Some("/appdata"), SUBPATH, "fb", "w");
        assert_eq!(
            path,
            PathBuf::from("/appdata/ShinraMeter-BPSR/logs/app.log")
        );
        assert!(warning.is_none());
    }

    #[test]
    fn the_subpath_hangs_under_appdata() {
        let (path, warning) = resolve(None, Some("/appdata"), SUBPATH, "fb", "w");
        assert_eq!(
            path,
            PathBuf::from("/appdata/ShinraMeter-BPSR/logs/app.log")
        );
        assert!(warning.is_none());
    }

    #[test]
    fn an_unset_or_empty_appdata_falls_back_and_warns() {
        for appdata in [None, Some("")] {
            let (path, warning) = resolve(None, appdata, SUBPATH, "fb", "w");
            assert_eq!(path, PathBuf::from("fb"));
            assert_eq!(warning.as_deref(), Some("w"));
        }
    }
}
