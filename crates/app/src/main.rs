//! `shinra-bpsr` — capture -> protocol -> meter -> overlay (plan §T4.2).
//!
//! A capture failure is never fatal: the overlay still runs and shows
//! `CaptureError::user_message()` in its status banner.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod pipeline;
mod ui;

use bpsr_protocol::ProtocolEvent;
use crossbeam_channel::bounded;
use ui::{OverlayApp, StatusLine, UiCommand};

/// Bounded so a stalled pipeline never grows unboundedly behind capture.
const EVENT_CAPACITY: usize = 4096;
const COMMAND_CAPACITY: usize = 64;

fn main() -> eframe::Result {
    env_logger::init();

    let (tx_events, rx_events) = bounded::<ProtocolEvent>(EVENT_CAPACITY);
    let (tx_command, rx_command) = bounded::<UiCommand>(COMMAND_CAPACITY);

    // Capture is best-effort: on failure `tx_events` is dropped, the pipeline
    // idles, and the overlay explains why.
    let (status, capture) = match bpsr_capture::start_capture(tx_events) {
        Ok(handle) => (StatusLine::Ok, Some(handle)),
        Err(err) => {
            log::error!("capture unavailable: {err}");
            (StatusLine::Error(err.user_message().to_string()), None)
        }
    };

    let (rx_snapshot, pipeline_thread) = pipeline::spawn(rx_events, rx_command);

    let native_options = eframe::NativeOptions {
        viewport: ui::viewport(),
        ..Default::default()
    };

    let result = eframe::run_native(
        "shinra-bpsr",
        native_options,
        Box::new(move |cc| {
            ui::apply_theme(&cc.egui_ctx);
            Ok(Box::new(
                OverlayApp::new(rx_snapshot, tx_command).with_status(status),
            ))
        }),
    );

    // Window closed: stop capture (drops its sender) and let the pipeline
    // thread wind down — the overlay's command sender is gone by now.
    if let Some(handle) = capture {
        handle.stop();
    }
    let _ = pipeline_thread.join();

    result
}
