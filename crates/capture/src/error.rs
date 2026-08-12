//! Capture error types shared by the (cfg-gated) platform backends.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("packet capture is not supported on this platform")]
    UnsupportedPlatform,
    #[error("WinDivert driver missing: {0}")]
    DriverMissing(String),
    #[error("not running as administrator")]
    NotElevated,
    #[error("failed to open capture device: {0}")]
    Open(String),
}

impl CaptureError {
    /// The exact text the UI shows for this error.
    pub fn user_message(&self) -> &'static str {
        match self {
            CaptureError::UnsupportedPlatform => "Packet capture is only supported on Windows.",
            CaptureError::DriverMissing(_) => {
                "WinDivert driver not found — place WinDivert.dll and WinDivert64.sys next to the exe."
            }
            CaptureError::NotElevated => "Run as Administrator.",
            CaptureError::Open(_) => "Failed to open the capture device.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_for_driver_missing_mentions_the_files() {
        let e = CaptureError::DriverMissing("not found".to_string());
        assert!(e.user_message().contains("WinDivert"));
    }

    #[test]
    fn user_message_for_not_elevated_mentions_administrator() {
        let e = CaptureError::NotElevated;
        assert!(e.user_message().contains("Administrator"));
    }
}
