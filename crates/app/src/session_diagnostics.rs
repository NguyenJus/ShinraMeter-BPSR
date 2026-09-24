//! Small cross-launch marker. Written only while the single-instance guard is held.
//! A missing completion means an interrupted run, not proof of a crash.
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    session: String,
    version: String,
    started_ms: u64,
    main_completed_ms: Option<u64>,
    joined_workers: Option<bool>,
}

pub struct SessionDiagnostics {
    path: PathBuf,
    marker: Marker,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_marker(path: &Path) -> io::Result<Marker> {
    let mut bytes = Vec::new();
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session marker exceeds 4096 bytes",
        ));
    }
    let marker: Marker = serde_json::from_slice(&bytes)?;
    // This is diagnostic metadata, never an arbitrary log-message source.
    if !marker
        .session
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'-')
        || marker.session.len() > 64
        || marker.version.len() > 64
        || marker.version.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid session marker",
        ));
    }
    Ok(marker)
}

fn write_marker(path: &Path, marker: &Marker) -> io::Result<()> {
    crate::paths::ensure_parent_dir(path)?;
    let pending = path.with_extension("json.pending");
    fs::write(&pending, serde_json::to_vec(marker)?)?;
    fs::rename(pending, path)
}

impl SessionDiagnostics {
    pub fn start() -> Self {
        let (path, warning) = crate::paths::resolve(
            None,
            std::env::var("APPDATA").ok().as_deref(),
            &["ShinraMeter-BPSR", "session-state.json"],
            "ShinraMeter-BPSR-session-state.json",
            "APPDATA unset; session marker uses working directory",
        );
        if let Some(warning) = warning {
            log::warn!("{warning}");
        }
        let started_ms = now_ms();
        match read_marker(&path) {
            Ok(previous) => log::info!(
                "session continuity: previous_session={} previous_version={} previous_main_completed_ms={:?} previous_joined_workers={:?} since_previous_start_ms={} current_session={}",
                previous.session,
                previous.version,
                previous.main_completed_ms,
                previous.joined_workers,
                started_ms.saturating_sub(previous.started_ms),
                crate::logging::session_id(),
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                log::info!("session continuity: no prior marker")
            }
            Err(err) => log::warn!("session continuity: prior marker unavailable ({err})"),
        }
        let result = Self {
            path,
            marker: Marker {
                session: crate::logging::session_id().to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                started_ms,
                main_completed_ms: None,
                joined_workers: None,
            },
        };
        result.save();
        result
    }

    fn save(&self) {
        if let Err(err) = write_marker(&self.path, &self.marker) {
            log::warn!("session continuity: marker write failed ({err})");
        }
    }

    /// This records reaching the end of main and the explicit worker joins.
    /// Capture's bounded stop has its own outcome log; this is not a clean-exit assertion.
    pub fn complete(mut self, joined_workers: bool) {
        self.marker.main_completed_ms = Some(now_ms());
        self.marker.joined_workers = Some(joined_workers);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_replacement_preserves_interruption_and_completion_distinction() {
        let dir =
            std::env::temp_dir().join(format!("shinra-session-marker-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut marker = Marker {
            session: "12-345".into(),
            version: "0.3.3".into(),
            started_ms: 100,
            main_completed_ms: None,
            joined_workers: None,
        };
        write_marker(&path, &marker).unwrap();
        assert_eq!(read_marker(&path).unwrap().main_completed_ms, None);
        marker.main_completed_ms = Some(200);
        marker.joined_workers = Some(false);
        write_marker(&path, &marker).unwrap();
        let loaded = read_marker(&path).unwrap();
        assert_eq!(loaded.main_completed_ms, Some(200));
        assert_eq!(loaded.joined_workers, Some(false));
        fs::write(&path, vec![b'x'; 4097]).unwrap();
        assert_eq!(
            read_marker(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
