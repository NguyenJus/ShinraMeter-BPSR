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
mod paths;
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
    let (path, warning) = paths::resolve(
        None,
        std::env::var("APPDATA").ok().as_deref(),
        &["ShinraMeter-BPSR", "names.json"],
        "ShinraMeter-BPSR-names.json",
        "APPDATA is not set; falling back to a working-directory file for the name cache",
    );
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }
    path
}

/// Issue #89: the wgpu configuration a *genuinely* transparent overlay needs on
/// Windows, or `None` when this machine can't provide it.
///
/// # The bug
///
/// The overlay opens as a transparent, undecorated, always-on-top window. DWM
/// allocates an opaque GDI *redirection surface* for such a window at
/// `CreateWindowEx`, sized to the window and initialized white; wgpu presents
/// with `alpha_mode: PreMultiplied`, so DWM blends the swapchain over that
/// white surface rather than over the desktop. The panel fill (18,18,22,200)
/// premultiplied over white lands on exactly the flat #454548 gray users
/// reported. Only area the window is later *grown* into lacks a redirection
/// surface, which is why dragging the edge appeared to "fix" the overlay one
/// freshly exposed strip at a time.
///
/// # The fix, and why it is all-or-nothing
///
/// `WS_EX_NOREDIRECTIONBITMAP` stops the surface from ever existing, but three
/// facts make it a package deal rather than a one-line flag:
///
/// 1. Windows honors the flag **only** at `CreateWindowEx`. Setting it later
///    with `SetWindowLongPtrW(GWL_EXSTYLE)` is silently ignored (measured on
///    the live window: zero pixels changed). Hence the vendored egui-winit in
///    `third_party/egui-winit` — see `set_no_redirection_bitmap` there.
/// 2. DXGI refuses `CreateSwapChainForHwnd` on a window carrying the flag, so
///    it must be paired with the DirectComposition presentation path
///    ([`wgpu::Dx12SwapchainKind::DxgiFromVisual`], which presents through an
///    `IDCompositionVisual` instead).
/// 3. That path is DX12-only. Vulkan is assumed unable to present to a
///    no-redirection window at all, so the flag must never be set unless the
///    DX12 backend is the one actually in use — which is why this pins
///    [`wgpu::Backends::DX12`] rather than merely preferring it.
///
/// # Degradation
///
/// Returning `None` leaves `NativeOptions::wgpu_options` at its default and
/// leaves the egui-winit opt-in untouched, i.e. exactly today's behaviour:
/// wgpu picks a backend as it always has and the overlay opens with the old
/// gray-until-resized startup. A machine without a DX12 adapter (a non-Windows
/// dev build, a stripped-down VM, an exotic driver setup) must still *launch* —
/// a cosmetic fix is never worth a failure to start.
fn dx12_composition_setup() -> Option<eframe::WgpuConfiguration> {
    use eframe::egui_wgpu::{WgpuSetup, wgpu};

    // A throwaway instance restricted to DX12: if it enumerates nothing, this
    // machine has no DX12 adapter and the whole scheme is off the table. Cheap
    // enough to do unconditionally at startup, and far safer than assuming
    // "Windows implies DX12". `enumerate_adapters` is async even on native
    // (wgpu 30), but the native future is already resolved, so `block_on`
    // returns immediately rather than parking.
    let probe = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    if pollster::block_on(probe.enumerate_adapters(wgpu::Backends::DX12)).is_empty() {
        return None;
    }

    let mut wgpu_options = eframe::WgpuConfiguration::default();
    // `WgpuSetup::Existing` would mean the caller already built an instance and
    // device, which `eframe::WgpuConfiguration::default()` never does — but
    // match rather than assume, and treat the impossible arm as "no DX12
    // configuration available" instead of panicking.
    let WgpuSetup::CreateNew(create_new) = &mut wgpu_options.wgpu_setup else {
        log::warn!(
            "eframe's default wgpu setup is no longer `CreateNew`; skipping the issue #89 DirectComposition configuration"
        );
        return None;
    };
    create_new.instance_descriptor.backends = wgpu::Backends::DX12;
    create_new
        .instance_descriptor
        .backend_options
        .dx12
        .presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
    Some(wgpu_options)
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

    // Issue #89. Both halves of the transparency fix are decided here, before
    // any window exists, because both are creation-time-only: the wgpu backend
    // choice is baked into the instance eframe builds, and
    // `WS_EX_NOREDIRECTIONBITMAP` is baked into `CreateWindowEx`. The two must
    // move together — the flag without DirectComposition gives a window DXGI
    // refuses to create a swapchain for, i.e. an overlay that never presents —
    // so the opt-in is flipped only inside the `Some` arm, never speculatively.
    let dx12_setup = dx12_composition_setup();
    if dx12_setup.is_some() {
        egui_winit::set_no_redirection_bitmap(true);
        log::info!(
            "DX12 adapter found: opening the overlay with WS_EX_NOREDIRECTIONBITMAP and DirectComposition presentation (issue #89)"
        );
    } else {
        log::info!(
            "no DX12 adapter: using eframe's default wgpu setup; the overlay may paint opaque until resized (issue #89)"
        );
    }

    let native_options = eframe::NativeOptions {
        viewport: ui::viewport(settings.window_position),
        // `unwrap_or_default()` is precisely "change nothing": it is the same
        // value `NativeOptions::default()` would have put here.
        wgpu_options: dx12_setup.unwrap_or_default(),
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
