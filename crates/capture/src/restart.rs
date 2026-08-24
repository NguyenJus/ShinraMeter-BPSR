//! The one-bit request channel that asks a running capture to re-anchor.
//!
//! Issue #214: `CaptureHandle::request_restart` existed but had no caller
//! anywhere, because the handle itself cannot leave the thread that owns it
//! — it holds a raw Windows `HANDLE`, which is neither `Send` nor `Sync`.
//! This is the part of it that *can* travel: a plain shared flag, `Send` and
//! `Sync` and cheap to clone, so the UI's command loop (and capture's own
//! stall detector) can ask for a restart without any of them holding the
//! driver handle.
//!
//! Lives outside `win.rs` so the app crate can name the type on every
//! platform, exactly as it names [`crate::CaptureHandle`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cloneable request to make capture forget the connection it is tracking
/// and re-run server detection from scratch.
///
/// Latching rather than counting: two requests that arrive before the
/// capture thread next wakes are one restart, which is what the user asking
/// twice should mean.
#[derive(Clone, Debug, Default)]
pub struct CaptureRestart(Arc<AtomicBool>);

impl CaptureRestart {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks for a restart. Never blocks and never fails: if the capture
    /// thread is gone, the flag simply goes unread.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Takes a pending request, clearing it. The capture loop calls this
    /// once per packet, so it must consume — a request left set would
    /// re-run detection on every packet forever.
    pub fn take_requested(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_requester_has_nothing_pending() {
        let restart = CaptureRestart::new();
        assert!(!restart.take_requested());
    }

    #[test]
    fn a_request_is_taken_exactly_once() {
        let restart = CaptureRestart::new();
        restart.request();
        assert!(restart.take_requested(), "the request must be visible");
        assert!(
            !restart.take_requested(),
            "taking it must consume it, or the capture loop would restart on every packet"
        );
    }

    /// The whole point of the type: the UI side holds a clone and the
    /// capture thread reads the same flag through its own.
    #[test]
    fn clones_share_one_flag() {
        let ui_side = CaptureRestart::new();
        let capture_side = ui_side.clone();
        ui_side.request();
        assert!(capture_side.take_requested());
        assert!(!ui_side.take_requested(), "the clone already took it");
    }
}
