//! `shinra-bpsr` — capture -> protocol -> meter -> overlay (plan §T4.2).
//!
//! A capture failure is never fatal: the overlay still runs and shows
//! `CaptureError::user_message()` in its status banner.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod fonts;
mod pipeline;
mod platform;
mod settings;
mod ui;

use std::path::PathBuf;

use bpsr_protocol::ProtocolEvent;
use crossbeam_channel::bounded;
use ui::{OverlayApp, StatusLine, UiCommand};

/// Bounded so a stalled pipeline never grows unboundedly behind capture.
const EVENT_CAPACITY: usize = 4096;
const COMMAND_CAPACITY: usize = 64;

/// Where the cross-session uid -> (name, class) cache (issue #12) lives:
/// `%APPDATA%\shinra-bpsr\names.json`. `bpsr-meter` deliberately knows
/// nothing about this path (it's caller-supplied, no Windows-specific
/// assumptions, no `directories` crate) — the app crate owns picking it.
/// Falls back to a current-directory file, logged, if `APPDATA` isn't set
/// (e.g. non-Windows dev/CI environments).
fn names_cache_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(appdata) if !appdata.is_empty() => PathBuf::from(appdata)
            .join("shinra-bpsr")
            .join("names.json"),
        _ => {
            log::warn!(
                "APPDATA is not set; falling back to a working-directory file for the name cache"
            );
            PathBuf::from("shinra-bpsr-names.json")
        }
    }
}

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

    let (rx_snapshot, pipeline_thread) = pipeline::spawn(rx_events, rx_command, names_cache_path());
    let (tx_settings, settings_thread) = settings::spawn_writer();

    // Kept alongside the clone handed to `OverlayApp` so shutdown can signal
    // the pipeline explicitly below, rather than depending on `run_native`
    // having already dropped `OverlayApp` (and with it its own sender) by
    // the time it returns.
    let tx_command_shutdown = tx_command.clone();

    let native_options = eframe::NativeOptions {
        viewport: ui::viewport(),
        ..Default::default()
    };

    let result = eframe::run_native(
        "shinra-bpsr",
        native_options,
        Box::new(move |cc| {
            fonts::install_cjk_fallback(&cc.egui_ctx);
            ui::apply_theme(&cc.egui_ctx);
            platform::disable_aero_snap(cc);
            Ok(Box::new(
                OverlayApp::new(rx_snapshot, tx_command, tx_settings).with_status(status),
            ))
        }),
    );

    // Window closed: stop capture (drops its sender) and tell the pipeline
    // thread to stop explicitly. `run_native` returns after closing the
    // window regardless of *how* it was closed (`UiCommand::Quit` via the
    // in-app button, alt-F4, or the window manager) — sending `Quit` here
    // guarantees a clean shutdown without relying on `OverlayApp`'s own
    // command sender having already been dropped.
    if let Some(handle) = capture {
        handle.stop();
    }
    let _ = tx_command_shutdown.try_send(UiCommand::Quit);
    let _ = pipeline_thread.join();
    // `OverlayApp` (and its `tx_settings`) is dropped by the time
    // `run_native` returns, which closes the settings-writer's channel and
    // lets its thread exit; joining here just makes sure the last-sent
    // settings value has finished being persisted before the process ends.
    let _ = settings_thread.join();

    result
}
