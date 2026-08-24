//! `ShinraMeter-BPSR` — capture -> protocol -> meter -> overlay (plan §T4.2).
//!
//! A capture failure is never fatal: the overlay still runs and shows
//! `CaptureError::user_message()` in its status banner.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use bpsr_app::{
    fonts, history, inspect, logging, paths, pipeline, platform, settings, single_instance, ui,
    update_check,
};
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

/// Where the encounter-history database (issue #39) lives:
/// `%APPDATA%\ShinraMeter-BPSR\history.sqlite`. `SHINRA_HISTORY_DB` overrides
/// it outright, because it is the file a developer most often wants pointed
/// at a scratch copy — though it is no longer the only override among the
/// app's on-disk files: the single-instance lock (issue #277) has its own,
/// `SHINRA_INSTANCE_LOCK` (see `single_instance::lock_file_path`).
fn history_db_path() -> PathBuf {
    let (path, warning) = paths::resolve(
        std::env::var("SHINRA_HISTORY_DB").ok().as_deref(),
        std::env::var("APPDATA").ok().as_deref(),
        &["ShinraMeter-BPSR", "history.sqlite"],
        "ShinraMeter-BPSR-history.sqlite",
        "APPDATA is not set; falling back to a working-directory file for the encounter history",
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

/// What `main` should do given the outcome of [`single_instance::acquire`]:
/// whether startup continues at all, and — if so — with what guard and what
/// warning (if any) to log on the way. Pulled out of `main()` as a pure
/// function (`Acquisition` in, `InstanceDecision` out; no logging, no
/// message box) so the decision issue #277 actually depends on — does a
/// second copy get told to stand down — is unit-testable without booting
/// the app, rather than only reachable by running `main()` end to end.
enum InstanceDecision {
    /// Continue starting up, holding this guard (if any) for the life of
    /// the process. `warning`, if set, is logged once by the caller.
    Continue {
        guard: Option<single_instance::InstanceGuard>,
        warning: Option<String>,
    },
    /// Another live instance already owns the meter slot: exit before
    /// capture ever opens a WinDivert handle.
    Exit,
}

fn decide_instance(acquisition: single_instance::Acquisition) -> InstanceDecision {
    match acquisition {
        single_instance::Acquisition::Acquired(guard) => InstanceDecision::Continue {
            guard: Some(guard),
            warning: None,
        },
        single_instance::Acquisition::AlreadyRunning => InstanceDecision::Exit,
        // A guard that cannot be evaluated must not be able to stop the app:
        // refusing to start because the *lock* is broken would be a worse
        // failure than the duplicate rows it exists to prevent.
        single_instance::Acquisition::Unavailable(reason) => InstanceDecision::Continue {
            guard: None,
            warning: Some(format!(
                "single-instance guard unavailable ({reason}); a second copy of the meter would go undetected (issue #277)"
            )),
        },
    }
}

fn main() -> eframe::Result {
    // `env_logger::init()` alone defaults to `error`-only and, since this
    // binary carries `windows_subsystem = "windows"`, has no console for
    // stderr to land on in a shipped build — so it was effectively silent.
    // `logging::init` (issue #69) turns logging on by default and tees it to
    // a file so a user hitting a bug can actually produce diagnostics.
    logging::init();

    // Issue #250: the previous in-place update, if there was one, left the
    // build it replaced beside the executable as `<exe>.old` — Windows will
    // not let a running image be deleted, so the process that installed the
    // update could not clean up after itself. This one can: whatever held
    // that file open is the process that exited to make room for this one.
    // Best-effort and never fatal (see the function's doc comment), and
    // deliberately after `logging::init` so a failure has somewhere to be
    // logged.
    update_check::clean_up_previous_update();

    // Issue #277: a second copy of the meter is never what the user meant.
    // Both instances append to the same log (every event line then appears
    // twice, from two independent capture loops) and both write the same
    // fight to `history.sqlite`, so the history list grows a duplicate row
    // per fight. Claimed before capture opens a WinDivert handle, so a
    // refused instance never touches the driver — and held in `_instance`
    // for the rest of `main`, because dropping the guard frees the slot.
    let _instance = match decide_instance(single_instance::acquire()) {
        InstanceDecision::Continue { guard, warning } => {
            if let Some(warning) = warning {
                log::warn!("{warning}");
            }
            guard
        }
        InstanceDecision::Exit => {
            log::error!("{}", single_instance::ALREADY_RUNNING_MESSAGE);
            platform::warn_already_running(single_instance::ALREADY_RUNNING_MESSAGE);
            return Ok(());
        }
    };

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

    // Loaded once, here, rather than inside `OverlayApp::new`: issue #27
    // needs this same value before `OverlayApp` exists, to seed
    // `ui::viewport`'s starting position, so the single load is hoisted up
    // to cover both uses instead of loading twice — and the history thread
    // needs the retention policy before the pipeline is spawned (issue #39).
    let settings = settings::load();

    // Issue #39: `None` when history is switched off in settings.json, or when
    // the database cannot be opened (already logged by `HistoryHandle::spawn`) —
    // in either case the overlay runs exactly as before, minus history.
    let history = settings
        .history_enabled
        .then(|| {
            history::writer::HistoryHandle::spawn(history_db_path(), settings.retention_policy())
        })
        .flatten();
    let (history_handle, history_thread) = match history {
        Some((handle, thread)) => (Some(handle), Some(thread)),
        None => (None, None),
    };

    // Issue #214: the pipeline thread drains the UI's command channel, so
    // it is what has to act on "Restart packet capture" — but the
    // `CaptureHandle` cannot go with it (raw Windows `HANDLE`, neither
    // `Send` nor `Sync`). Hand over the shared flag instead; `None` when
    // capture failed to start, in which case the banner above already says
    // why and there is nothing to restart.
    let capture_restart = capture.as_ref().map(|handle| handle.restart_requester());

    let (rx_snapshot, pipeline_thread) = pipeline::spawn(
        rx_events,
        rx_command,
        names_cache_path(),
        history_handle.clone(),
        capture_restart,
    );
    let (tx_settings, settings_thread) = settings::spawn_writer();

    // Kept alongside the clone handed to `OverlayApp` so shutdown can signal
    // the pipeline explicitly below, rather than depending on `run_native`
    // having already dropped `OverlayApp` (and with it its own sender) by
    // the time it returns.
    let tx_command_shutdown = tx_command.clone();

    // Issue #53: the tray's "Reset Window" command restores the overlay to
    // the size `ui::viewport` opens at on a first launch. Read back off a
    // default builder rather than re-deriving it, so `ui`'s private layout
    // constants stay private and the two can't drift apart.
    let default_inner_size = ui::viewport(None, None).inner_size;

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
        viewport: ui::viewport(settings.window_position, settings.window_size),
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
                OverlayApp::new(
                    rx_snapshot,
                    tx_command,
                    tx_settings,
                    settings,
                    history_handle,
                )
                .with_status(status),
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
    // Issue #39: both `HistoryHandle` clones are gone by now — the
    // pipeline's, joined just above, and `OverlayApp`'s own (moved into
    // `OverlayApp::new` above, not merely cloned into it), dropped when
    // `run_native` returned, before capture was even stopped — so the
    // history thread's channel is closed and it exits after draining.
    // Joining here is what guarantees the session's last encounter actually
    // reached disk — the same explicit-shutdown discipline
    // `CacheWriter::shutdown` follows.
    if let Some(thread) = history_thread {
        let _ = thread.join();
    }
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

    // -- decide_instance (issue #277) ----------------------------------------
    //
    // `main()` itself is untestable end to end, but the decision that
    // matters for issue #277 — does a second copy get told to stand down —
    // is this match, extracted into a pure function precisely so a refactor
    // that drops the early exit or reorders it after `start_capture` fails
    // a test instead of silently reintroducing the bug.

    fn instance_lock_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shinra-main-decide-instance-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("instance.lock")
    }

    #[test]
    fn acquired_continues_and_keeps_the_guard() {
        let acquisition = single_instance::acquire_at(&instance_lock_path("acquired"));
        match decide_instance(acquisition) {
            InstanceDecision::Continue { guard, warning } => {
                assert!(guard.is_some(), "the winning guard must be kept alive");
                assert!(warning.is_none());
            }
            InstanceDecision::Exit => panic!("expected Continue"),
        }
    }

    #[test]
    fn already_running_exits() {
        let path = instance_lock_path("already_running");
        let _first = single_instance::acquire_at(&path);
        let second = single_instance::acquire_at(&path);
        assert!(
            matches!(decide_instance(second), InstanceDecision::Exit),
            "a second live instance must be told to exit, not to continue"
        );
    }

    #[test]
    fn unavailable_continues_without_a_guard_and_logs_the_reason() {
        let acquisition = single_instance::Acquisition::Unavailable("disk full".to_string());
        match decide_instance(acquisition) {
            InstanceDecision::Continue { guard, warning } => {
                assert!(guard.is_none(), "a broken guard must not be held");
                let warning = warning.expect("an unavailable guard should still warn");
                assert!(warning.contains("disk full"));
            }
            InstanceDecision::Exit => panic!("a broken guard must not stop the app (issue #277)"),
        }
    }
}
