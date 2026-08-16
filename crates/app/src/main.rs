//! `ShinraMeter-BPSR` — capture -> protocol -> meter -> overlay (plan §T4.2).
//!
//! A capture failure is never fatal: the overlay still runs and shows
//! `CaptureError::user_message()` in its status banner.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod dump;
mod fonts;
mod icons;
mod inspect;
mod logging;
mod pipeline;
mod platform;
mod settings;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use bpsr_protocol::ProtocolEvent;
use crossbeam_channel::bounded;
use ui::{OverlayApp, StatusLine, UiCommand};

/// Bounded so a stalled pipeline never grows unboundedly behind capture.
const EVENT_CAPACITY: usize = 4096;
const COMMAND_CAPACITY: usize = 64;

/// Where the cross-session uid -> (name, class) cache (issue #12) lives:
/// `%APPDATA%\ShinraMeter-BPSR\names.json`. `bpsr-meter` deliberately knows
/// nothing about this path (it's caller-supplied, no Windows-specific
/// assumptions, no `directories` crate) — the app crate owns picking it.
/// Falls back to a current-directory file, logged, if `APPDATA` isn't set
/// (e.g. non-Windows dev/CI environments).
fn names_cache_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(appdata) if !appdata.is_empty() => PathBuf::from(appdata)
            .join("ShinraMeter-BPSR")
            .join("names.json"),
        _ => {
            log::warn!(
                "APPDATA is not set; falling back to a working-directory file for the name cache"
            );
            PathBuf::from("ShinraMeter-BPSR-names.json")
        }
    }
}

fn main() -> eframe::Result {
    // `env_logger::init()` alone defaults to `error`-only and, since this
    // binary carries `windows_subsystem = "windows"`, has no console for
    // stderr to land on in a shipped build — so it was effectively silent.
    // `logging::init` (issue #69) turns logging on by default and tees it to
    // a file so a user hitting a bug can actually produce diagnostics.
    logging::init();

    let (tx_events, rx_events) = bounded::<ProtocolEvent>(EVENT_CAPACITY);
    let (tx_command, rx_command) = bounded::<UiCommand>(COMMAND_CAPACITY);

    // Opt-in packet-inspection diagnostics (issue #25 slice A); `None` unless
    // `SHINRA_INSPECT` is set, in which case `start_capture` below wires the
    // sink into its decoder.
    let inspect_handle = inspect::init();
    let inspect_sink = inspect_handle.as_ref().map(|h| Arc::clone(&h.sink));

    // Capture is best-effort: on failure `tx_events` is dropped, the pipeline
    // idles, and the overlay explains why.
    let (status, capture) = match bpsr_capture::start_capture(tx_events, inspect_sink) {
        Ok(handle) => (StatusLine::Ok, Some(handle)),
        Err(err) => {
            log::error!("capture unavailable: {err}");
            (StatusLine::Error(err.user_message().to_string()), None)
        }
    };

    let (rx_snapshot, pipeline_thread) = pipeline::spawn(rx_events, rx_command, names_cache_path());
    let (tx_settings, settings_thread) = settings::spawn_writer();

    // Loaded once, here, rather than inside `OverlayApp::new`: issue #27
    // needs this same value before `OverlayApp` exists, to seed
    // `ui::viewport`'s starting position, so the single load is hoisted up
    // to cover both uses instead of loading twice.
    let settings = settings::load();

    // Kept alongside the clone handed to `OverlayApp` so shutdown can signal
    // the pipeline explicitly below, rather than depending on `run_native`
    // having already dropped `OverlayApp` (and with it its own sender) by
    // the time it returns.
    let tx_command_shutdown = tx_command.clone();

    // Issue #53: the tray's "Reset Window" command restores the overlay to
    // the size `ui::viewport` opens at on a first launch. Read back off a
    // default builder rather than re-deriving it, so `ui`'s private layout
    // constants stay private and the two can't drift apart.
    let default_inner_size = ui::viewport(None).inner_size;

    let native_options = eframe::NativeOptions {
        viewport: ui::viewport(settings.window_position),
        ..Default::default()
    };

    let result = eframe::run_native(
        "ShinraMeter-BPSR",
        native_options,
        Box::new(move |cc| {
            fonts::install_cjk_fallback(&cc.egui_ctx);
            ui::apply_theme(&cc.egui_ctx);
            platform::disable_aero_snap(cc);
            platform::install_snap_blocker(cc);
            platform::install_tray(cc, default_inner_size);
            platform::clamp_window_to_visible_area(cc);
            Ok(Box::new(
                OverlayApp::new(rx_snapshot, tx_command, tx_settings, settings).with_status(status),
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
    // Capture has already stopped above, so its `Decoder`'s reference to the
    // sink is gone by now — this drops the last one, which is what lets
    // `DiagnosticSink`'s summary actually log (see `inspect::Handle::shutdown`).
    if let Some(inspect_handle) = inspect_handle {
        inspect_handle.shutdown();
    }

    result
}
