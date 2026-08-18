//! `ShinraMeter-BPSR` — capture -> protocol -> meter -> overlay (plan §T4.2).
//!
//! A capture failure is never fatal: the overlay still runs and shows
//! `CaptureError::user_message()` in its status banner.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod assets;
mod dump;
mod fonts;
mod icons;
// IMAGINE-TAKEDOWN: one of five sites — see `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
mod imagines;
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

/// Issue #89: how the overlay window and its swapchain get created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowComposition {
    /// `WS_EX_NOREDIRECTIONBITMAP` paired with DirectComposition presentation
    /// on DX12 — the genuinely transparent overlay.
    DirectComposition,
    /// Held back by `SHINRA_NO_COMPOSITION`.
    ForcedOff,
    /// No hardware DX12 adapter to present through.
    NoDx12Adapter,
}

impl WindowComposition {
    /// Why the legacy opaque path was taken, for the startup log line. The
    /// `DirectComposition` arm is reachable only when the choice was made and
    /// [`dx12_composition_setup`] then failed to build the configuration.
    fn reason(self) -> &'static str {
        match self {
            Self::DirectComposition => "the DirectComposition configuration could not be built",
            Self::ForcedOff => "held back by SHINRA_NO_COMPOSITION",
            Self::NoDx12Adapter => "no hardware DX12 adapter",
        }
    }
}

/// Issue #89: the transparency decision, with both inputs passed in so the
/// branching is testable without a GPU or a Windows box — the raw
/// `SHINRA_NO_COMPOSITION` value, and a probe for a hardware DX12 adapter.
///
/// The escape hatch wins, and short-circuits the probe: `set_no_redirection_bitmap`
/// is only reached if this returns [`WindowComposition::DirectComposition`], and
/// `WS_EX_NOREDIRECTIONBITMAP` is baked into `CreateWindowEx` and cannot be
/// taken back on the live window. A machine where the DirectComposition path
/// leaves the overlay black or blank — a broken or ancient driver, an RDP
/// session, a virtualized GPU — therefore has no in-app way out; the only
/// recovery is refusing the path before the window exists. The probe is skipped
/// too, so a driver that hangs or crashes inside adapter enumeration is
/// recoverable by the same variable.
///
/// Opt-out truthiness rule: set to anything but empty, `0` or `false` counts
/// as on (i.e. as "no composition"). Unlike `SHINRA_INSPECT` (opt-in since
/// issue #122), this variable is still opt-out — an unset or off-looking
/// value leaves the DirectComposition fix enabled.
fn composition_choice(
    no_composition: Option<&str>,
    hardware_dx12_adapter: impl FnOnce() -> bool,
) -> WindowComposition {
    let forced_off = no_composition.is_some_and(|value| {
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    });
    if forced_off {
        WindowComposition::ForcedOff
    } else if hardware_dx12_adapter() {
        WindowComposition::DirectComposition
    } else {
        WindowComposition::NoDx12Adapter
    }
}

/// Whether this machine has a *hardware* DX12 adapter, the prerequisite for the
/// configuration [`dx12_composition_setup`] builds.
///
/// Software adapters (WARP, "Microsoft Basic Render Driver") do enumerate under
/// DX12 in exactly the environments where a composition swapchain is least
/// likely to work — RDP sessions, virtualized or absent GPUs — so they do not
/// count: a machine whose only DX12 adapter is a CPU one keeps the old opaque
/// startup rather than risking a window that never presents at all. That is the
/// cheapest capability signal wgpu offers here; short of building a
/// `DxgiFromVisual` swapchain for real (which needs a window, i.e. after the
/// irreversible `CreateWindowEx`), there is nothing better to test.
///
/// Both the start and the end of the probe are logged. `enumerate_adapters` is
/// async even on native (wgpu 30) and the native future is already resolved, so
/// the `block_on` below returns immediately rather than parking — but this
/// binary carries `windows_subsystem = "windows"`, so if that claim were ever
/// wrong the app would hang before its first window with no console and nothing
/// to go on. `logging::init` runs first in `main`, so the "probing" line is
/// already on disk if the line after it never arrives.
fn hardware_dx12_adapter() -> bool {
    use eframe::egui_wgpu::wgpu;

    // A throwaway instance restricted to DX12: if it enumerates nothing, this
    // machine has no DX12 adapter and the whole scheme is off the table. Cheap
    // enough to do unconditionally at startup, and far safer than assuming
    // "Windows implies DX12".
    log::info!("probing for a hardware DX12 adapter (issue #89)");
    let probe = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapters = pollster::block_on(probe.enumerate_adapters(wgpu::Backends::DX12));
    let found: Vec<String> = adapters
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            format!("{} ({:?})", info.name, info.device_type)
        })
        .collect();
    let hardware = adapters
        .iter()
        .any(|adapter| adapter.get_info().device_type != wgpu::DeviceType::Cpu);
    log::info!(
        "DX12 probe finished: {} adapter(s) [{}]; hardware adapter present: {hardware}",
        adapters.len(),
        found.join(", ")
    );
    hardware
}

/// Issue #89: the wgpu configuration a *genuinely* transparent overlay needs on
/// Windows, or `None` when eframe's defaults no longer offer the seam it needs.
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
/// Not taking this path leaves `NativeOptions::wgpu_options` at its default and
/// leaves the egui-winit opt-in untouched, i.e. exactly today's behaviour:
/// wgpu picks a backend as it always has and the overlay opens with the old
/// gray-until-resized startup. A machine without a hardware DX12 adapter (a
/// non-Windows dev build, a stripped-down VM, an exotic driver setup) must
/// still *launch* — a cosmetic fix is never worth a failure to start, which is
/// also why `SHINRA_NO_COMPOSITION` exists (see [`composition_choice`]).
fn dx12_composition_setup() -> Option<eframe::WgpuConfiguration> {
    use eframe::egui_wgpu::{WgpuSetup, wgpu};

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

    // Opt-in packet-inspection diagnostics (issue #25 slice A, opt-in default
    // since issue #122); `None` unless `SHINRA_INSPECT` opts in (see
    // `inspect::enabled`), in which case `start_capture` below wires the
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
    let choice = composition_choice(
        std::env::var("SHINRA_NO_COMPOSITION").ok().as_deref(),
        hardware_dx12_adapter,
    );
    let dx12_setup = match choice {
        WindowComposition::DirectComposition => dx12_composition_setup(),
        WindowComposition::ForcedOff | WindowComposition::NoDx12Adapter => None,
    };
    if dx12_setup.is_some() {
        egui_winit::set_no_redirection_bitmap(true);
        log::info!(
            "opening the overlay with WS_EX_NOREDIRECTIONBITMAP and DirectComposition presentation (issue #89); if it stays black or blank, set SHINRA_NO_COMPOSITION=1 and restart to get the old opaque window back"
        );
    } else {
        log::info!(
            "using eframe's default wgpu setup ({}); the overlay may paint opaque gray until resized (issue #89)",
            choice.reason()
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    // -- composition_choice (issue #89) -------------------------------------

    #[test]
    fn composition_uses_direct_composition_with_a_hardware_dx12_adapter() {
        assert_eq!(
            composition_choice(None, || true),
            WindowComposition::DirectComposition
        );
    }

    #[test]
    fn composition_falls_back_without_a_hardware_dx12_adapter() {
        assert_eq!(
            composition_choice(None, || false),
            WindowComposition::NoDx12Adapter
        );
    }

    /// The escape hatch has to beat the probe, not just its result: a driver
    /// that hangs or crashes inside adapter enumeration is one of the things
    /// `SHINRA_NO_COMPOSITION` exists to rescue.
    #[test]
    fn the_escape_hatch_forces_the_legacy_path_without_probing() {
        let probed = Cell::new(false);
        let choice = composition_choice(Some("1"), || {
            probed.set(true);
            true
        });
        assert_eq!(choice, WindowComposition::ForcedOff);
        assert!(!probed.get(), "SHINRA_NO_COMPOSITION must skip the probe");
    }

    /// An off-looking value must not disable the fix — `SHINRA_NO_COMPOSITION`
    /// is opt-out, so `SHINRA_NO_COMPOSITION=0` means "no, don't".
    #[test]
    fn an_off_looking_escape_hatch_value_leaves_the_fix_on() {
        for value in ["", "0", "false", "FALSE"] {
            assert_eq!(
                composition_choice(Some(value), || true),
                WindowComposition::DirectComposition,
                "SHINRA_NO_COMPOSITION={value:?}"
            );
        }
    }

    #[test]
    fn every_legacy_path_explains_itself_in_the_startup_log() {
        for choice in [
            WindowComposition::DirectComposition,
            WindowComposition::ForcedOff,
            WindowComposition::NoDx12Adapter,
        ] {
            assert!(!choice.reason().is_empty(), "{choice:?} has no reason");
        }
    }
}
