//! Non-Windows stub.
//!
//! Packet capture is implemented against WinDivert, which only exists on
//! Windows. This module exposes the same public API as [`crate::win`] so
//! `crates/app` can call `start_capture` unconditionally, but it always
//! reports [`CaptureError::UnsupportedPlatform`].

use bpsr_protocol::ProtocolEvent;
use crossbeam_channel::Sender;

use crate::error::CaptureError;

/// No-op handle returned by [`start_capture`] on non-Windows platforms.
pub struct CaptureHandle;

impl CaptureHandle {
    /// No-op: there is no running capture thread to restart.
    pub fn request_restart(&self) {}

    /// No-op: there is no running capture thread to stop.
    pub fn stop(self) {}
}

/// Always fails with [`CaptureError::UnsupportedPlatform`] on non-Windows
/// platforms — packet capture requires the WinDivert driver.
pub fn start_capture(_tx: Sender<ProtocolEvent>) -> Result<CaptureHandle, CaptureError> {
    Err(CaptureError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_capture_returns_unsupported_platform() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        match start_capture(tx) {
            Err(CaptureError::UnsupportedPlatform) => {}
            Err(other) => panic!("expected UnsupportedPlatform, got {other:?}"),
            Ok(_) => panic!("expected an error on non-Windows"),
        }
    }

    #[test]
    fn user_message_explains_windows_only() {
        let err = CaptureError::UnsupportedPlatform;
        assert!(err.user_message().contains("Windows"));
    }

    #[test]
    fn handle_methods_are_no_ops() {
        let handle = CaptureHandle;
        handle.request_restart();
        handle.stop();
    }
}
