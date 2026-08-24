//! Non-Windows stub.
//!
//! Packet capture is implemented against WinDivert, which only exists on
//! Windows. This module exposes the same public API as [`crate::win`] so
//! `crates/app` can call `start_capture` unconditionally, but it always
//! reports [`CaptureError::UnsupportedPlatform`].

use std::sync::Arc;

use bpsr_protocol::{InspectSink, ProtocolEvent};
use crossbeam_channel::Sender;

use crate::error::CaptureError;
use crate::restart::CaptureRestart;

/// No-op handle returned by [`start_capture`] on non-Windows platforms.
pub struct CaptureHandle;

impl CaptureHandle {
    /// No-op: there is no running capture thread to restart.
    pub fn request_restart(&self) {}

    /// A detached requester: nothing reads it, because there is no capture
    /// thread on this platform. Exists so `crates/app` can wire the header
    /// dropdown's "Restart packet capture" item unconditionally, exactly as
    /// it calls `start_capture` unconditionally.
    pub fn restart_requester(&self) -> CaptureRestart {
        CaptureRestart::new()
    }

    /// No-op: there is no running capture thread to stop.
    pub fn stop(self) {}
}

/// Always fails with [`CaptureError::UnsupportedPlatform`] on non-Windows
/// platforms — packet capture requires the WinDivert driver. `_inspect_sink`
/// (issue #25 slice A) is accepted only to keep this signature matching
/// `win::start_capture`'s; there is no decoder here to wire it into.
pub fn start_capture(
    _tx: Sender<ProtocolEvent>,
    _inspect_sink: Option<Arc<dyn InspectSink>>,
) -> Result<CaptureHandle, CaptureError> {
    Err(CaptureError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_capture_returns_unsupported_platform() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        match start_capture(tx, None) {
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
        assert!(
            !handle.restart_requester().take_requested(),
            "the stub's requester is detached: nothing reads it and nothing sets it"
        );
        handle.stop();
    }
}
