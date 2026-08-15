//! Windows-only startup tweaks that egui/winit don't expose a cross-platform
//! way to do.
//!
//! Issue #11: Windows' Snap subsystem only engages inside the OS's own
//! modal move/resize loops, which `ViewportCommand::StartDrag` (`SC_MOVE`)
//! and `ViewportCommand::BeginResize` (`SC_SIZE`) used to enter. `ui.rs`
//! no longer uses either: `ui::draw_header` and `ui::draw_resize_handles`
//! now track the pointer themselves and drive the window with
//! `ViewportCommand::OuterPosition`/`InnerSize`, so no native loop — and
//! therefore no drag-triggered Snap — is ever entered. The three defenses
//! below are kept as belt-and-braces for the paths that never went
//! through a drag at all.
//!
//! The *drag-triggered* snap path is driven by the `WS_MAXIMIZEBOX` window
//! style, not by whether the window has decorations, so `disable_aero_snap`
//! clears that bit directly on the raw `HWND` right after the window is
//! created (and, since a `GWL_STYLE` change alone doesn't take visible
//! effect, forces a frame-change notification so DWM actually picks it up).
//!
//! That style bit doesn't reach every path to Snap, though: Win+Arrow
//! keyboard snap and the shell's own maximize command are shell/DWM-
//! initiated, not drag gestures, and ignore `WS_MAXIMIZEBOX` entirely. The
//! owner wants Snap disabled outright, so `install_snap_blocker` adds a
//! second, message-level defense — a `SetWindowSubclass` hook (chained with
//! `DefSubclassProc`, not a `GWLP_WNDPROC` swap) that rejects snap-shaped
//! `WM_WINDOWPOSCHANGING` proposals and swallows `WM_SYSCOMMAND`/
//! `SC_MAXIMIZE` as a backstop. See its doc comment, and `should_veto_snap`'s
//! and `is_snap_shaped`'s, for the actual decision logic.
//!
//! `WS_THICKFRAME` is untouched by any of this, so the window stays
//! resizable; the subclass tells a legitimate app-driven reposition apart
//! from a shell-initiated Snap by an exemption flag with two scopes:
//! `app_driven_reposition` for a single `SetWindowPos` call (this file's
//! own `clamp_window_to_visible_area`), and `begin_app_driven_reposition`
//! for a whole `ui.rs` move/resize gesture, whose queued viewport commands
//! winit turns into `SetWindowPos` calls of its own long after any closure
//! would have returned.
//!
//! Not verified against a real Windows session: these workarounds can only
//! be confirmed by actually dragging/Win+Arrow-snapping the overlay near a
//! screen edge on Windows, which isn't possible from this (Linux,
//! cross-compiling) environment. Only the pure `is_snap_shaped` geometry is
//! exercised by tests here, the same way `corrected_position` is below.
//!
//! Issue #30: `clamp_window_to_visible_area` below is a second, unrelated
//! startup tweak that lives here for the same reason — it needs the raw
//! `HWND` and Win32 monitor APIs egui/eframe don't expose. See its own doc
//! comment and `corrected_position`'s for what it does and why. Likewise
//! not verified against a real Windows session; only the pure
//! `corrected_position` geometry is exercised by tests here.
//!
//! Issue #53: `install_tray` at the bottom is a third one — minimizing sends
//! the overlay to the notification area instead of the taskbar. It lives
//! here for the same reason again (`Shell_NotifyIconW` and the popup-menu
//! APIs have no egui/eframe equivalent) and reuses this module's `Rect`,
//! monitor enumeration and `app_driven_reposition` directly. See its doc
//! comment for the message flow and the lifetime rules for the icon.

/// Bit for `WS_MAXIMIZEBOX` in a Win32 window style (`GWL_STYLE`) bitmask.
/// Kept as a plain literal — rather than referencing
/// `windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX` — so this
/// module (and its masking logic) stays unit-testable off-Windows.
/// `disable_aero_snap` below `debug_assert`s this matches the windows-crate
/// constant.
#[cfg(any(windows, test))]
const WS_MAXIMIZEBOX_BIT: isize = 0x0001_0000;

/// Clears `WS_MAXIMIZEBOX` from a `GWL_STYLE` bitmask, leaving every other
/// bit (notably `WS_THICKFRAME`, which manual edge-drag resizing needs, and
/// `WS_VISIBLE`/`WS_POPUP`, which keep the overlay showing at all)
/// untouched. Pure so the masking arithmetic is unit-testable without a
/// real `HWND`.
#[cfg(any(windows, test))]
fn without_maximize_box(style: isize) -> isize {
    style & !WS_MAXIMIZEBOX_BIT
}

#[cfg(windows)]
pub fn disable_aero_snap(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_MAXIMIZEBOX,
    };

    debug_assert_eq!(WS_MAXIMIZEBOX.0 as isize, WS_MAXIMIZEBOX_BIT);

    let handle = match cc.window_handle() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!(
                "couldn't get the overlay's window handle; Aero Snap may still trigger: {err:?}"
            );
            return;
        }
    };

    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        log::warn!("overlay window handle isn't a Win32 handle; skipping the Aero Snap workaround");
        return;
    };

    let hwnd = HWND(win32.hwnd.get());
    // SAFETY: `hwnd` is winit's own handle for the window that owns this
    // `CreationContext`, which is alive and on the current thread for the
    // duration of this call.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    if style == 0 {
        // A live window always has a nonzero style, so a 0 return here means
        // the call failed. Bail out instead of feeding 0 into the mask below
        // — `0 & !WS_MAXIMIZEBOX_BIT` is still 0, and setting GWL_STYLE to 0
        // would clear WS_VISIBLE/WS_POPUP/etc. and hide the overlay.
        log::warn!(
            "GetWindowLongPtrW(GWL_STYLE) returned 0 (likely failed); skipping the Aero Snap workaround to avoid zeroing the window style"
        );
        return;
    }

    let new_style = without_maximize_box(style);
    // SAFETY: same as above.
    let prev = unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new_style) };
    if prev == 0 {
        // Since `style` above was confirmed nonzero, a successful call here
        // returns that same nonzero previous style; 0 means it failed.
        log::warn!("SetWindowLongPtrW(GWL_STYLE) failed; Aero Snap may still trigger on drag");
        return;
    }

    // A `GWL_STYLE` change alone doesn't make DWM recompute the window's
    // frame — Win32 requires an explicit `SWP_FRAMECHANGED` `SetWindowPos`
    // for that. Without this, DWM keeps compositing the client area
    // against its own opaque redirection surface instead of honoring the
    // per-pixel alpha winit set up via `DwmEnableBlurBehindWindow`, so the
    // whole window paints opaque gray at startup until something else
    // (e.g. a resize) forces a frame update. `SWP_NOMOVE | SWP_NOSIZE`
    // means this call only asks for the frame-change side effect, not an
    // actual move/resize, so the position/size arguments are ignored.
    //
    // Wrapped in `app_driven_reposition` like every other direct
    // `SetWindowPos` in this crate. The wrapper is inert *today* — this runs
    // before `install_snap_blocker` in `main.rs`, and `SWP_NOMOVE |
    // SWP_NOSIZE` would trip `window_proc`'s `already_pinned` check even if
    // it didn't — but the invariant is the point: reordering `main.rs` (a
    // "reset window" feature re-running this, say), or copying this call as
    // a template for one that really does move the window, must not
    // reintroduce an unwrapped, vetoable call.
    // SAFETY: same as above.
    let framechanged = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            HWND(0),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    });
    if !framechanged.as_bool() {
        log::warn!(
            "SetWindowPos(SWP_FRAMECHANGED) failed; the window may keep painting opaque until resized"
        );
    }

    log::info!("cleared WS_MAXIMIZEBOX to disable Aero Snap on drag");
}

#[cfg(not(windows))]
pub fn disable_aero_snap(_cc: &eframe::CreationContext<'_>) {}

/// A monitor's or window's rectangle in physical pixels / virtual-screen
/// coordinates — so a monitor to the left of or above the primary one has
/// negative `left`/`top`, never remapped to `[0, size]`. Left/top are
/// inclusive, right/bottom exclusive, matching Win32's `RECT` convention.
/// Hand-rolled rather than reusing `windows::Win32::Foundation::RECT` so
/// `corrected_position` below has no windows-crate types in its signature
/// and can be unit-tested off-Windows.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(any(windows, test))]
impl Rect {
    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }

    fn center(self) -> (i32, i32) {
        (self.left + self.width() / 2, self.top + self.height() / 2)
    }
}

/// Height, in physical pixels, of the drag band `ui::draw_header` actually
/// registers at the top of the overlay window. The overlay is borderless
/// (`ui::viewport`) and that band is the *only* way the user can move it,
/// which is what makes "is enough of it reachable" the right test below
/// instead of "is the whole window on screen". This file never converts
/// between points and physical pixels or accounts for per-monitor DPI — it
/// stays entirely in the same physical-pixel, virtual-screen space Win32
/// itself uses (`GetWindowRect`, `MONITORINFO.rcWork`) — so this is a
/// deliberately approximate stand-in for `ui.rs`'s point-based header
/// height, not a scaled match to it; only "comfortably smaller than a real
/// header" matters here.
#[cfg(any(windows, test))]
const HEADER_BAND_HEIGHT: i32 = 32;

/// Minimum width, in physical pixels, of the header band that must land
/// inside some monitor's work area for a saved position to count as
/// recoverable by dragging.
#[cfg(any(windows, test))]
const MIN_VISIBLE_WIDTH: i32 = 160;

/// Minimum height, in physical pixels, of the header band that must land
/// inside some monitor's work area for a saved position to count as
/// recoverable. Deliberately smaller than `HEADER_BAND_HEIGHT` itself:
/// requiring the *full* band height would reject positions where the
/// window is shifted up so only the band's lower pixels show, even though
/// that's still enough to grab and drag.
#[cfg(any(windows, test))]
const MIN_VISIBLE_HEIGHT: i32 = 24;

/// Whether at least `MIN_VISIBLE_WIDTH` x `MIN_VISIBLE_HEIGHT` physical
/// pixels of the overlay's header `band` land inside the *union* of the
/// monitors' work areas.
///
/// Measuring per-monitor would be wrong: two edge-adjacent monitors (say
/// x∈[0, 1920) and x∈[1920, 3840)) can each cover only half of a band that
/// is nonetheless fully on screen and perfectly draggable, and judging that
/// band unreachable would force-relocate a deliberately placed window. So
/// the monitors' horizontal contributions to the band are coalesced first,
/// and the test is whether any single contiguous run of that union is at
/// least `MIN_VISIBLE_WIDTH` wide.
///
/// Pure integer-rect geometry with no windows-crate types, for the same
/// unit-testability reason as `Rect` itself.
#[cfg(any(windows, test))]
fn band_is_reachable(band: Rect, monitors: &[Rect]) -> bool {
    // Only monitors that overlap the band vertically by at least
    // MIN_VISIBLE_HEIGHT contribute a horizontal span. Applying that filter
    // *before* merging is the conservative rule the merge depends on:
    // otherwise two monitors stacked so each covers a different partial
    // sliver of the band's height would be merged horizontally as if either
    // one covered the band's full height, which neither does.
    let mut spans: Vec<(i32, i32)> = monitors
        .iter()
        .filter(|m| band.bottom.min(m.bottom) - band.top.max(m.top) >= MIN_VISIBLE_HEIGHT)
        .filter_map(|m| {
            let left = band.left.max(m.left);
            let right = band.right.min(m.right);
            (right > left).then_some((left, right))
        })
        .collect();

    // Coalesce into contiguous runs. Spans that merely touch
    // (`start == end`) count as contiguous, since edge-adjacent monitors
    // share a boundary coordinate with no gap between them; a real gap in
    // the layout leaves `start > end` and correctly breaks the run.
    spans.sort_unstable();
    let mut widest = 0;
    let mut run: Option<(i32, i32)> = None;
    for (start, end) in spans {
        run = match run {
            Some((run_start, run_end)) if start <= run_end => Some((run_start, run_end.max(end))),
            Some((run_start, run_end)) => {
                widest = widest.max(run_end - run_start);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((run_start, run_end)) = run {
        widest = widest.max(run_end - run_start);
    }

    widest >= MIN_VISIBLE_WIDTH
}

/// The strip of `window` that `ui::draw_header` registers as the drag
/// surface: the top `HEADER_BAND_HEIGHT` physical pixels across the full
/// width.
#[cfg(any(windows, test))]
fn header_band(window: Rect) -> Rect {
    Rect {
        left: window.left,
        top: window.top,
        right: window.right,
        bottom: window.top + HEADER_BAND_HEIGHT,
    }
}

/// Whether `window` is somewhere the user can still grab and drag it on the
/// current monitor layout — i.e. enough of its header band lands inside the
/// union of the monitors' work areas (see `band_is_reachable`).
///
/// Used at both ends of this module: `corrected_position` decides whether a
/// *restored* position needs correcting, and `should_veto_snap` decides
/// whether pinning the window where it currently sits is safe.
#[cfg(any(windows, test))]
fn window_is_reachable(window: Rect, monitors: &[Rect]) -> bool {
    band_is_reachable(header_band(window), monitors)
}

/// Issue #30: decides whether `window`'s current position needs correcting
/// given the monitors currently attached (`monitors`, each that monitor's
/// *work area* — `MONITORINFO.rcWork`, excluding the taskbar, since a
/// header hidden under the taskbar is exactly as unreachable as one off the
/// edge of the display), and if so, where to move it. All geometry is
/// physical pixels / virtual-screen coordinates.
///
/// # Why "is the header band reachable" rather than "is the window on
/// screen"
///
/// The overlay is borderless, and the drag surface in `ui::draw_header` —
/// the top `HEADER_BAND_HEIGHT` px across the window's full width — is the
/// *only* way the user can move it (which it does by tracking the pointer
/// itself, not via any OS move loop). So "acceptable" isn't "fully on some monitor": a window sitting
/// mostly off a monitor's edge, with just enough header showing to grab, is
/// already recoverable by the user, and correcting it anyway would fight a
/// deliberate multi-monitor layout (e.g. parked mostly off-screen in a
/// corner on purpose). The bar is instead: does at least `MIN_VISIBLE_WIDTH`
/// x `MIN_VISIBLE_HEIGHT` physical pixels of that band overlap the *union*
/// of the monitors' work areas — see `band_is_reachable`, which measures
/// against the union rather than each monitor alone so a band straddling
/// two edge-adjacent monitors isn't wrongly judged unreachable.
///
/// # Why the nearest monitor when it doesn't
///
/// When no monitor clears that bar, the window moves to whichever
/// monitor's work-area center is closest (squared distance) to the
/// window's own center, clamped to fit inside that monitor's work area.
/// The failure mode this exists for is "the monitor this was saved on is
/// gone, or the layout changed", so reappearing on the nearest surviving
/// monitor reads as "it moved" rather than "an arbitrary monitor was
/// picked for me".
///
/// Returns `None` when no correction is needed — the common case — so the
/// caller can skip `SetWindowPos` entirely.
#[cfg(any(windows, test))]
fn corrected_position(window: Rect, monitors: &[Rect]) -> Option<(i32, i32)> {
    if monitors.is_empty() {
        // No information to correct against: doing nothing is safer than
        // guessing. The caller logs a warning when this happens, since it
        // only occurs if monitor enumeration itself failed.
        return None;
    }

    if window_is_reachable(window, monitors) {
        return None;
    }

    let (wx, wy) = window.center();
    // Squared distance as `i64` so the `i32` deltas can't overflow it.
    let nearest = monitors
        .iter()
        .min_by_key(|m| {
            let (mx, my) = m.center();
            let dx = i64::from(mx - wx);
            let dy = i64::from(my - wy);
            dx * dx + dy * dy
        })
        .expect("monitors is non-empty, checked above");

    let clamp = |value: i32, lo: i32, hi: i32| -> i32 {
        // `hi` can end up less than `lo` when the window is bigger than the
        // work area on this axis (e.g. a huge saved size restored onto a
        // small monitor) — pin to `lo` in that case rather than calling
        // `i32::clamp` with an inverted range, which panics.
        if hi < lo { lo } else { value.clamp(lo, hi) }
    };
    let x = clamp(window.left, nearest.left, nearest.right - window.width());
    let y = clamp(window.top, nearest.top, nearest.bottom - window.height());
    Some((x, y))
}

/// Issue #30: `Settings.window_position` is restored verbatim into
/// `ViewportBuilder::with_position` (`ui::viewport`, called from
/// `main.rs`) with no validation. If the monitor it was saved on is gone,
/// or the monitor layout changed since, the window can reopen at
/// coordinates on no visible display — invisible, and because the only way
/// to move a borderless overlay is dragging its header
/// (`ui::draw_header`), also unreachable; the only recovery today is
/// hand-editing settings.json.
///
/// This runs at `CreationContext` time, alongside `disable_aero_snap`
/// above, so any correction happens before the first paint — no visible
/// jump, no "wait a frame then check" flag needed.
///
/// Not solved through egui/eframe: no app-facing hook receives a winit
/// event loop or monitor list at `ViewportBuilder` time, and
/// `egui::ViewportInfo::monitor_size` only exposes the *current* monitor's
/// size in points, with no origin and no list of the others — not enough
/// to validate a position, since a window at a negative x on a monitor to
/// the left of primary is perfectly valid and a `[0, monitor_size]` clamp
/// would wrongly yank it back. So this goes straight to the raw `HWND` and
/// Win32 monitor APIs, the same way `disable_aero_snap` does.
///
/// See `corrected_position` for the actual decision (what counts as
/// "reachable", and where an unreachable window goes instead) — this
/// function is just the Win32 plumbing: read the window's real rect,
/// enumerate monitors' work areas, and apply whatever `corrected_position`
/// decides.
#[cfg(windows)]
pub fn clamp_window_to_visible_area(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };

    let handle = match cc.window_handle() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!(
                "couldn't get the overlay's window handle; skipping the position clamp: {err:?}"
            );
            return;
        }
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        log::warn!("overlay window handle isn't a Win32 handle; skipping the position clamp");
        return;
    };
    let hwnd = HWND(win32.hwnd.get());

    let mut win_rect = RECT::default();
    // SAFETY: `hwnd` is winit's own handle for the window that owns this
    // `CreationContext`, alive and on the current thread; `win_rect` is a
    // valid, correctly-sized out-param for the duration of this call.
    let got_rect = unsafe { GetWindowRect(hwnd, &mut win_rect) };
    if !got_rect.as_bool() {
        log::warn!("GetWindowRect failed; skipping the position clamp");
        return;
    }
    let window = Rect {
        left: win_rect.left,
        top: win_rect.top,
        right: win_rect.right,
        bottom: win_rect.bottom,
    };

    let monitors = enumerate_monitor_work_areas();
    if monitors.is_empty() {
        log::warn!("no monitors enumerated; skipping the position clamp");
        return;
    }

    let Some((x, y)) = corrected_position(window, &monitors) else {
        return;
    };
    log::info!(
        "overlay position ({}, {}) isn't reachable on the current monitor layout; moving it to ({x}, {y})",
        window.left,
        window.top
    );
    // Wrapped in `app_driven_reposition` so the snap-blocking subclass
    // (`install_snap_blocker`) never mistakes this app-initiated
    // correction for a shell-driven Snap and vetoes it.
    // SAFETY: same as `GetWindowRect` above. `SWP_NOSIZE` means `cx`/`cy`
    // are ignored, and `SWP_NOZORDER` means the second handle is ignored.
    let moved = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            HWND(0),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    });
    if !moved.as_bool() {
        log::warn!("SetWindowPos failed while correcting the overlay's position");
    }
}

#[cfg(not(windows))]
pub fn clamp_window_to_visible_area(_cc: &eframe::CreationContext<'_>) {}

/// `EnumDisplayMonitors` callback used by `clamp_window_to_visible_area`:
/// appends each monitor's work area (`MONITORINFO.rcWork`, i.e. excluding
/// the taskbar) to the `Vec<Rect>` whose address was passed through
/// `lparam`.
#[cfg(windows)]
unsafe extern "system" fn monitor_enum_callback(
    hmonitor: windows::Win32::Graphics::Gdi::HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _clip_rect: *mut windows::Win32::Foundation::RECT,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::BOOL {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `hmonitor` is a live handle `EnumDisplayMonitors` owns for
    // the duration of this callback; `info` is a correctly-sized out-param.
    if unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() {
        // SAFETY: `lparam` was set by `clamp_window_to_visible_area`, our
        // only caller, to the address of a `Vec<Rect>` local that outlives
        // this entire `EnumDisplayMonitors` call; `EnumDisplayMonitors`
        // invokes this callback synchronously and single-threaded, so
        // there's no aliasing and no reentrancy on that pointer.
        let monitors = unsafe { &mut *(lparam.0 as *mut Vec<Rect>) };
        monitors.push(Rect {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
        });
    }
    true.into()
}

/// Shared by `clamp_window_to_visible_area` above and `window_proc` below:
/// the current monitors' work areas (`MONITORINFO.rcWork`, excluding the
/// taskbar), via the same `EnumDisplayMonitors` + `monitor_enum_callback`
/// pairing `clamp_window_to_visible_area` used before this was extracted.
#[cfg(windows)]
fn enumerate_monitor_work_areas() -> Vec<Rect> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC};

    let mut monitors: Vec<Rect> = Vec::new();
    // SAFETY: see `monitor_enum_callback`'s doc comment above — same
    // synchronous, single-threaded, non-retaining call.
    let enumerated = unsafe {
        EnumDisplayMonitors(
            HDC(0),
            None,
            Some(monitor_enum_callback),
            LPARAM(std::ptr::addr_of_mut!(monitors) as isize),
        )
    };
    if !enumerated.as_bool() {
        // Keep going with whatever the callback did collect: a partial
        // list still beats an empty one; callers guard against `is_empty`.
        log::warn!("EnumDisplayMonitors failed; the monitor list may be partial");
    }
    monitors
}

/// Wraps a *single* app-initiated `SetWindowPos` call — this module's own
/// `clamp_window_to_visible_area` above — so the snap-blocking subclass
/// installed by `install_snap_blocker` can tell "the app moved itself"
/// apart from a shell/DWM-initiated Windows Snap and only veto the latter.
///
/// Every direct `SetWindowPos` call anywhere in this crate MUST be
/// wrapped in this. An unwrapped call risks being rejected by
/// `window_proc`'s `WM_WINDOWPOSCHANGING` handling below whenever it
/// happens to propose a snap-shaped rect — e.g. this module's own
/// position clamp landing on exactly half the screen.
///
/// This closure form is *not* enough for the manual move/resize gestures
/// in `ui.rs`: those don't call `SetWindowPos` themselves, they queue
/// `ViewportCommand::OuterPosition`/`InnerSize`, which winit applies
/// later in the frame — long after any closure here would have returned.
/// Those use `begin_app_driven_reposition` below instead, which holds the
/// same exemption open across the whole gesture.
///
/// The exemption is process-wide, not per-window, so keep `f` scoped
/// tightly to the `SetWindowPos` call(s) that need to bypass the guard,
/// not unrelated work that happens to run alongside it.
///
/// Gated the same way the geometry helpers are: every caller is a
/// `#[cfg(windows)]` `SetWindowPos` site (`ui.rs` uses the guard form
/// instead), so an ungated definition is dead code on a Linux host build.
#[cfg(any(windows, test))]
pub fn app_driven_reposition<R>(f: impl FnOnce() -> R) -> R {
    let _guard = begin_app_driven_reposition();
    f()
}

/// Opens an app-driven-reposition exemption that lasts until the returned
/// guard is dropped, rather than for one closure call. This is what a
/// manual move/resize gesture in `ui.rs` holds for its whole duration:
/// the `ViewportCommand`s it queues are applied by winit later in the
/// frame, so every `SetWindowPos` winit issues in between has to be
/// exempt, not just the moment the command was queued.
///
/// Dropping the guard is the *only* way the exemption is released, so the
/// caller must make sure that happens on every exit path — pointer
/// released, drag cancelled, window unfocused. A leaked guard leaves
/// `window_proc`'s snap veto disabled for the rest of the session.
#[must_use = "the exemption lasts exactly as long as the returned guard"]
pub fn begin_app_driven_reposition() -> RepositionGuard {
    APP_DRIVEN_REPOSITION_DEPTH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    RepositionGuard { _private: () }
}

/// RAII handle for the exemption `begin_app_driven_reposition` opens.
#[derive(Debug)]
pub struct RepositionGuard {
    /// Keeps the guard unconstructible outside this module, so the only
    /// way to get one is the `fetch_add` in `begin_app_driven_reposition`
    /// that its `Drop` pairs with.
    _private: (),
}

impl Drop for RepositionGuard {
    fn drop(&mut self) {
        APP_DRIVEN_REPOSITION_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether any exemption is currently open; `window_proc` skips its snap
/// veto while this holds.
#[cfg(any(windows, test))]
fn app_driven_reposition_active() -> bool {
    APP_DRIVEN_REPOSITION_DEPTH.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// How many `RepositionGuard`s are alive. A refcount rather than a flag so
/// a short closure-scoped exemption nested inside a long gesture-scoped
/// one doesn't clear the outer one's on the way out.
static APP_DRIVEN_REPOSITION_DEPTH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Tolerance, in physical pixels, when matching a proposed rect against a
/// monitor-half/quarter/full shape in `is_snap_shaped`. Windows computes
/// snap rects by splitting the work area's width/height, which can land a
/// pixel off from a naive `/2` when that dimension is odd; this absorbs
/// that rounding without being loose enough to false-positive on a
/// deliberate user resize that merely lands close to (but isn't) a
/// half/quarter.
#[cfg(any(windows, test))]
const SNAP_SHAPE_TOLERANCE: i32 = 2;

#[cfg(any(windows, test))]
fn approx_eq(a: i32, b: i32) -> bool {
    (a - b).abs() <= SNAP_SHAPE_TOLERANCE
}

#[cfg(any(windows, test))]
fn rects_approx_eq(a: Rect, b: Rect) -> bool {
    approx_eq(a.left, b.left)
        && approx_eq(a.top, b.top)
        && approx_eq(a.right, b.right)
        && approx_eq(a.bottom, b.bottom)
}

/// Whether `proposed` — a candidate window rect taken from a
/// `WM_WINDOWPOSCHANGING` message — matches the shape Windows Snap (Win+
/// Arrow, drag-to-edge, or maximize) would produce on any of `monitors`:
/// a half (left/right/top/bottom), a quarter (one of the four corners),
/// or a monitor's full work area. `window_proc` below vetoes proposals
/// this returns `true` for, unless they're app-driven (see
/// `app_driven_reposition`).
///
/// Pure geometry, no windows-crate types, so it's unit-testable off
/// Windows — the actual Win32 message plumbing lives in `window_proc`.
#[cfg(any(windows, test))]
fn is_snap_shaped(proposed: Rect, monitors: &[Rect]) -> bool {
    monitors.iter().any(|m| {
        let mid_x = m.left + m.width() / 2;
        let mid_y = m.top + m.height() / 2;
        let shapes = [
            *m,
            Rect {
                left: m.left,
                top: m.top,
                right: mid_x,
                bottom: m.bottom,
            }, // left half
            Rect {
                left: mid_x,
                top: m.top,
                right: m.right,
                bottom: m.bottom,
            }, // right half
            Rect {
                left: m.left,
                top: m.top,
                right: m.right,
                bottom: mid_y,
            }, // top half
            Rect {
                left: m.left,
                top: mid_y,
                right: m.right,
                bottom: m.bottom,
            }, // bottom half
            Rect {
                left: m.left,
                top: m.top,
                right: mid_x,
                bottom: mid_y,
            }, // top-left quarter
            Rect {
                left: mid_x,
                top: m.top,
                right: m.right,
                bottom: mid_y,
            }, // top-right quarter
            Rect {
                left: m.left,
                top: mid_y,
                right: mid_x,
                bottom: m.bottom,
            }, // bottom-left quarter
            Rect {
                left: mid_x,
                top: mid_y,
                right: m.right,
                bottom: m.bottom,
            }, // bottom-right quarter
        ];
        shapes.iter().any(|shape| rects_approx_eq(proposed, *shape))
    })
}

/// Whether `window_proc` should countermand a `WM_WINDOWPOSCHANGING`
/// proposal (`proposed`) for a window currently at `current`, given the
/// monitors currently attached.
///
/// # Why shape alone isn't enough
///
/// `is_snap_shaped` is pure geometry: it can't tell a real Snap from any
/// other non-app-driven `SetWindowPos` that happens to land on a monitor
/// half/quarter/full rect. The case that actually bites is Windows' own
/// orphaned-window recovery: unplug the monitor the overlay was living on
/// and the shell repositions it onto a surviving display. If that target
/// coincidentally matches a half or quarter, vetoing it pins the window
/// where it is — off-screen, on a monitor that no longer exists, with no
/// header left to grab and no way back short of hand-editing settings.json.
///
/// So the veto also requires that pinning the window is *survivable*: the
/// rect it would be pinned at has to still be reachable (`window_is_reachable`
/// — the same "can the user grab the header band" test `corrected_position`
/// uses at startup). A window that is already stranded has nothing to lose
/// by accepting the reposition, and everything to lose by refusing it.
///
/// This is deliberately ordering-independent rather than keyed off
/// `WM_DISPLAYCHANGE`: the recovery `SetWindowPos` can arrive either side of
/// that notification, but by the time it does the dead monitor is already
/// out of `enumerate_monitor_work_areas`, so the reachability test sees the
/// stranding either way. It also needs no timer and no suspension state that
/// could leak and disable the veto for the rest of the session.
///
/// # Residual gap
///
/// A non-Snap, non-app-driven reposition of a window that *is* currently
/// reachable, onto a rect that happens to match a half/quarter within
/// `SNAP_SHAPE_TOLERANCE`, is still vetoed. Closing that would need a
/// positive signal that a Snap gesture is in flight, and none exists for the
/// paths this has to block: `WM_SYSCOMMAND`/`SC_MOVE`/`SC_SIZE` and
/// `WM_ENTERSIZEMOVE` mark the OS's modal move/resize loops, which Win+Arrow
/// and shell maximize never enter (and which `ui.rs`'s pointer-delta header
/// drag and resize handles deliberately never enter either, so gating on
/// them would block nothing and break nothing — it would just turn the veto
/// off). The consequence of the remaining false positive is bounded: the
/// window stays put somewhere the user can still drag it.
#[cfg(any(windows, test))]
fn should_veto_snap(current: Rect, proposed: Rect, monitors: &[Rect]) -> bool {
    is_snap_shaped(proposed, monitors) && window_is_reachable(current, monitors)
}

/// Subclass ID passed to `SetWindowSubclass`; unique among any subclasses
/// this crate installs on the same `HWND` (there's currently only this
/// one).
#[cfg(windows)]
const SNAP_BLOCKER_SUBCLASS_ID: usize = 1;

/// Installs a `SetWindowSubclass` hook — chained with `DefSubclassProc`,
/// never a direct `GWLP_WNDPROC` swap, since that would break winit's own
/// message pump (focus, close, DPI-change, ...) — that blocks Windows
/// Snap end to end:
///
/// - `WM_WINDOWPOSCHANGING`: `disable_aero_snap`'s `WS_MAXIMIZEBOX` clear
///   only stops the *drag-triggered* Aero Snap path. Win+Arrow keyboard
///   snap and drag-to-edge are shell/DWM-initiated and don't consult that
///   style bit at all — they show up here as a `WINDOWPOS` proposal whose
///   rect happens to be a monitor half/quarter (`is_snap_shaped`, reusing
///   `enumerate_monitor_work_areas`, the same monitor enumeration
///   `clamp_window_to_visible_area` uses) *and* the window is currently
///   somewhere refusing the move still leaves it draggable
///   (`should_veto_snap`, which keeps this from stranding the overlay when
///   the shell is rescuing it off a monitor that just got unplugged).
///   Legitimate app-driven repositioning is exempted via
///   `app_driven_reposition` (one `SetWindowPos` call) or
///   `begin_app_driven_reposition` (a whole `ui.rs` move/resize gesture) —
///   every such site must hold one, or it risks being vetoed here.
/// - `WM_SYSCOMMAND` / `SC_MAXIMIZE`: a backstop for the shell's maximize
///   command directly, independent of whatever rect it would propose.
///
/// Neither handler touches focus or z-order, so always-on-top and the
/// game keeping focus are unaffected; every other message falls through
/// to `DefSubclassProc` unchanged.
#[cfg(windows)]
pub fn install_snap_blocker(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;

    let handle = match cc.window_handle() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!("couldn't get the overlay's window handle; Snap may still trigger: {err:?}");
            return;
        }
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        log::warn!(
            "overlay window handle isn't a Win32 handle; skipping the Snap-blocking subclass"
        );
        return;
    };
    let hwnd = HWND(win32.hwnd.get());

    // SAFETY: `hwnd` is winit's own handle for the window that owns this
    // `CreationContext`, alive and on the current thread; `window_proc` is
    // a valid `SUBCLASSPROC` for the lifetime of the window (Windows
    // removes all subclasses automatically when the window is destroyed,
    // so there's no matching `RemoveWindowSubclass` needed here).
    let installed =
        unsafe { SetWindowSubclass(hwnd, Some(window_proc), SNAP_BLOCKER_SUBCLASS_ID, 0) };
    if !installed.as_bool() {
        log::warn!("SetWindowSubclass failed; keyboard/shell Snap may still trigger");
        return;
    }
    log::info!("installed the window-message subclass that blocks Snap");
}

#[cfg(not(windows))]
pub fn install_snap_blocker(_cc: &eframe::CreationContext<'_>) {}

/// `SetWindowSubclass` callback installed by `install_snap_blocker`; see
/// its doc comment for what each message does and why.
#[cfg(windows)]
unsafe extern "system" fn window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _uidsubclass: usize,
    _dwrefdata: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SC_MAXIMIZE, SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS, WM_SYSCOMMAND,
        WM_WINDOWPOSCHANGING,
    };

    if msg == WM_WINDOWPOSCHANGING && !app_driven_reposition_active() {
        // SAFETY: for `WM_WINDOWPOSCHANGING`, `lparam` is a valid pointer
        // to a `WINDOWPOS` the sender owns for the duration of this call;
        // mutating it in place before forwarding to `DefSubclassProc` is
        // exactly how a handler is meant to countermand a proposed
        // move/resize.
        let windowpos = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
        // If the proposal is already pinned on move and/or size, it isn't
        // a full move+resize and so can't be a snap shape (Snap always
        // both moves and resizes) — nothing to veto.
        let already_pinned = (windowpos.flags & (SWP_NOMOVE | SWP_NOSIZE)).0 != 0;
        if !already_pinned {
            let proposed = Rect {
                left: windowpos.x,
                top: windowpos.y,
                right: windowpos.x + windowpos.cx,
                bottom: windowpos.y + windowpos.cy,
            };
            // The veto pins the window at wherever it currently sits, so
            // where that is decides whether refusing is safe — see
            // `should_veto_snap` for the reasoning and for the residual
            // false-positive window this still leaves open.
            // SAFETY: `hwnd` is the window this subclass is installed on,
            // alive and on the current thread for the duration of this
            // callback; `current` is a valid, correctly-sized out-param.
            let mut current = RECT::default();
            if !unsafe { GetWindowRect(hwnd, &mut current) }.as_bool() {
                // Without the current rect there's no way to tell a veto
                // from a stranding, and letting a Snap through is the
                // cheaper mistake of the two.
                log::warn!(
                    "GetWindowRect failed in the Snap blocker; letting this reposition through"
                );
            } else {
                let current = Rect {
                    left: current.left,
                    top: current.top,
                    right: current.right,
                    bottom: current.bottom,
                };
                if should_veto_snap(current, proposed, &enumerate_monitor_work_areas()) {
                    windowpos.flags = windowpos.flags | SWP_NOMOVE | SWP_NOSIZE;
                }
            }
        }
    } else if msg == WM_SYSCOMMAND {
        // The low 4 bits of a WM_SYSCOMMAND wParam can carry a mouse/key
        // source flag; mask them off before comparing, per the SC_* usage
        // convention.
        let cmd = (wparam.0 & 0xFFF0) as u32;
        if cmd == SC_MAXIMIZE {
            // Swallow it outright rather than forwarding: returning 0
            // without calling DefSubclassProc is how WM_SYSCOMMAND
            // handlers signal "handled, don't do the default action".
            return windows::Win32::Foundation::LRESULT(0);
        }
    }

    // SAFETY: `hwnd`/`msg`/`wparam`/`lparam` are exactly what this
    // subclass procedure was called with; forwarding them unchanged to
    // `DefSubclassProc` is how every message this handler doesn't
    // explicitly veto keeps reaching winit's own window procedure.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Subclass ID for the notification-area subclass (issue #53), distinct
/// from `SNAP_BLOCKER_SUBCLASS_ID` above. A second subclass rather than
/// more branches in `window_proc`: the two have nothing to say to each
/// other, both chain through `DefSubclassProc`, and keeping them separate
/// means the Snap veto's message handling can't be perturbed by tray work.
#[cfg(windows)]
const TRAY_SUBCLASS_ID: usize = 2;

/// `uID` of the one notification-area icon this process owns. The
/// (`hWnd`, `uID`) pair is the shell's identity for a tray icon, so every
/// `NIM_ADD`/`NIM_DELETE` below has to use this same value.
#[cfg(windows)]
const TRAY_ICON_UID: u32 = 1;

/// Private message the shell posts to the overlay's `HWND` for every tray
/// mouse event. `WM_APP`-based (rather than `WM_USER`-based) because
/// `WM_USER` ids are only unique per window *class*, and this subclass is
/// installed on a window class winit owns and may itself use `WM_USER`
/// messages on.
#[cfg(windows)]
const TRAY_CALLBACK_MESSAGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// Menu-item id for "Reset Window" in the tray context menu. Ids start at 1
/// because `TrackPopupMenu(TPM_RETURNCMD)` returns 0 to mean "the menu was
/// dismissed without a selection".
#[cfg(any(windows, test))]
const TRAY_MENU_RESET_WINDOW: u32 = 1;

/// Menu-item id for "Exit" in the tray context menu.
#[cfg(any(windows, test))]
const TRAY_MENU_EXIT: u32 = 2;

/// Windows message ids the shell packs into the `lParam` of
/// `TRAY_CALLBACK_MESSAGE`. Kept as plain literals — rather than the
/// `windows::Win32::UI::WindowsAndMessaging` constants — so
/// `tray_action_for` below stays unit-testable off-Windows, the same way
/// `WS_MAXIMIZEBOX_BIT` is. `install_tray` `debug_assert`s each against the
/// windows-crate constant.
#[cfg(any(windows, test))]
const WM_LBUTTONUP_ID: u32 = 0x0202;
#[cfg(any(windows, test))]
const WM_LBUTTONDBLCLK_ID: u32 = 0x0203;
#[cfg(any(windows, test))]
const WM_RBUTTONUP_ID: u32 = 0x0205;
#[cfg(any(windows, test))]
const WM_CONTEXTMENU_ID: u32 = 0x007B;

/// What a click on the tray icon should do.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAction {
    /// Pop the context menu with "Reset Window" / "Exit".
    ShowMenu,
    /// Un-hide the overlay where it already is.
    Restore,
}

/// Maps the mouse message the shell packed into a `TRAY_CALLBACK_MESSAGE`
/// `lParam` to the action it triggers, or `None` for the many messages
/// (moves, button-downs, hover/balloon notifications) the tray icon
/// deliberately ignores.
///
/// Right-button *up*, not down, so the menu appears when the gesture
/// completes — the shell sends both, and acting on the down would leave the
/// subsequent up to fall through to the freshly-opened menu. `WM_CONTEXTMENU`
/// is the keyboard route to the same thing (the menu key while the tray icon
/// has focus), which the shell delivers only for that path, so handling both
/// double-fires nothing.
///
/// Pure so the mapping is unit-testable off-Windows; the Win32 plumbing is
/// in `tray_window_proc`.
#[cfg(any(windows, test))]
fn tray_action_for(mouse_message: u32) -> Option<TrayAction> {
    match mouse_message {
        WM_RBUTTONUP_ID | WM_CONTEXTMENU_ID => Some(TrayAction::ShowMenu),
        WM_LBUTTONUP_ID | WM_LBUTTONDBLCLK_ID => Some(TrayAction::Restore),
        _ => None,
    }
}

/// What the user picked from the tray context menu.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayCommand {
    /// Restore the overlay to its default size and a centered position — the
    /// recovery path issue #53 asks for when the window has been dragged or
    /// resized somewhere the user can't find it again.
    ResetWindow,
    /// Close the overlay, which runs `main.rs`'s normal shutdown.
    Exit,
}

/// Maps a `TrackPopupMenu(TPM_RETURNCMD)` return value to the command it
/// selected. `0` — "dismissed without picking anything" — and any id this
/// module never appended both map to `None`.
///
/// Pure so the mapping is unit-testable off-Windows.
#[cfg(any(windows, test))]
fn tray_command_for(menu_id: u32) -> Option<TrayCommand> {
    match menu_id {
        TRAY_MENU_RESET_WINDOW => Some(TrayCommand::ResetWindow),
        TRAY_MENU_EXIT => Some(TrayCommand::Exit),
        _ => None,
    }
}

/// Whether the notification-area icon is currently registered with the
/// shell. The tray icon exists *only* while the overlay is hidden (see
/// `install_tray`'s doc comment for why that choice over a permanent icon),
/// so this doubles as "is the overlay currently minimized to the tray".
///
/// A `static` rather than the subclass's `dwRefData` for the same reason
/// `APP_DRIVEN_REPOSITION_DEPTH` is one: every reader and writer is on the
/// single UI thread, and a plain atomic needs no unsafe pointer round-trip.
#[cfg(windows)]
static TRAY_ICON_ADDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The id `RegisterWindowMessageW("TaskbarCreated")` returned at
/// `install_tray` time, or `0` if that call failed (or hasn't run).
///
/// Explorer restarting — a crash, or the user killing it — destroys every
/// notification-area icon and broadcasts this message so owners can put
/// theirs back. Without handling it, an overlay that was minimized when
/// Explorer went down would be hidden, taskbar-less and icon-less: exactly
/// the unreachable-window state the rest of this module exists to prevent.
/// The id is only known at runtime (the shell allocates it in the
/// `0xC000..=0xFFFF` system range), so it can't be a `const` and has to be
/// compared against `msg` dynamically.
#[cfg(windows)]
static TASKBAR_CREATED_MESSAGE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The overlay's default inner size in egui points, packed by `pack_size`,
/// as captured at `install_tray` time. `0` means "never set", which
/// `unpack_size` reports as `None` and "Reset Window" degrades to a
/// reposition-only recovery for.
#[cfg(windows)]
static DEFAULT_INNER_SIZE_PT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Packs an inner size (egui points) into the single `u64` the subclass
/// reads it back out of. Two `f32` bit patterns rather than rounded
/// integers so the value survives the round trip exactly; a zero or
/// negative extent packs to something `unpack_size` rejects, which is what
/// makes `0` usable as the "unset" sentinel.
#[cfg(any(windows, test))]
fn pack_size(width: f32, height: f32) -> u64 {
    (u64::from(width.to_bits()) << 32) | u64::from(height.to_bits())
}

/// Inverse of `pack_size`, rejecting anything that isn't a usable size —
/// the `0` sentinel, a NaN, or a non-positive extent — so callers can't
/// resize the window to nothing.
#[cfg(any(windows, test))]
fn unpack_size(packed: u64) -> Option<(f32, f32)> {
    let width = f32::from_bits((packed >> 32) as u32);
    let height = f32::from_bits(packed as u32);
    (width > 0.0 && height > 0.0).then_some((width, height))
}

/// Where "Reset Window" puts the overlay: `size` (physical pixels), clamped
/// to fit `work_area`, centered in it.
///
/// Centering rather than restoring a remembered position: this is the
/// recovery path for "the window is somewhere I can't get at it", so the
/// one placement guaranteed to be both visible and obvious is the middle of
/// the monitor. `ui::viewport` has no default position of its own to
/// restore — on a first launch it leaves placement to winit — so there is no
/// existing constant to reuse here.
///
/// Pure integer-rect geometry, unit-testable off-Windows like the rest of
/// this module's geometry.
#[cfg(any(windows, test))]
fn default_window_rect(work_area: Rect, size: (i32, i32)) -> Rect {
    let width = size.0.clamp(1, work_area.width().max(1));
    let height = size.1.clamp(1, work_area.height().max(1));
    let left = work_area.left + (work_area.width() - width) / 2;
    let top = work_area.top + (work_area.height() - height) / 2;
    Rect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Issue #53: makes minimizing send the overlay to the notification area
/// instead of the taskbar.
///
/// # How the minimize is intercepted
///
/// `ui.rs`'s minimize button queues `ViewportCommand::Minimized(true)`,
/// which winit turns into a plain `ShowWindow(SW_MINIMIZE)` — not a
/// `WM_SYSCOMMAND`/`SC_MINIMIZE`, so hooking that would catch nothing. What
/// *is* guaranteed to arrive either way is `WM_SIZE` with `SIZE_MINIMIZED`,
/// so the subclass installed here waits for that, lets winit process it
/// normally, and only then hides the window. Hooking the message rather than
/// the button means every route to a minimized overlay (the in-app button,
/// Win+D, a shell "minimize all") lands in the tray, and `ui.rs` needs no
/// call-site change.
///
/// `ShowWindow(SW_HIDE)` is the part that removes the taskbar entry:
/// `viewport()` never sets `with_taskbar(false)`, so a merely-minimized
/// window keeps its button. If adding the icon fails the window is
/// deliberately left minimized-but-visible rather than hidden — a hidden
/// window with no tray icon and no taskbar button would be unreachable.
///
/// # Icon lifetime
///
/// The icon exists only while the overlay is hidden, and is deleted the
/// moment it's restored. The alternative — a permanent icon for the whole
/// session — would mean a tray entry that does nothing 99% of the time for
/// an app that is already always-on-top and visible. `NIM_DELETE` therefore
/// runs from three places: restore, "Reset Window", and `WM_NCDESTROY`
/// (which covers exit, including exit *from* the tray, so a ghost icon can't
/// outlive the process). The other end of that lifetime — the shell dropping
/// the icon out from under us on an Explorer restart — is handled by
/// `TASKBAR_CREATED_MESSAGE`; see its doc comment.
///
/// `default_inner_size` is `ui::viewport`'s own default inner size in egui
/// points, threaded through from `main.rs` so the "Reset Window" command can
/// restore exactly that size without `ui`'s private layout constants leaking
/// out of that module. `None` (which `ui::viewport` never actually produces)
/// degrades "Reset Window" to a recenter at the current size.
///
/// Not verified against a real Windows session, for the same reason as
/// everything else in this file; only the pure `tray_action_for`,
/// `tray_command_for`, `pack_size`/`unpack_size` and `default_window_rect`
/// logic is exercised by tests here.
#[cfg(windows)]
pub fn install_tray(cc: &eframe::CreationContext<'_>, default_inner_size: Option<egui::Vec2>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;
    use windows::Win32::UI::WindowsAndMessaging::{
        RegisterWindowMessageW, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
    };

    debug_assert_eq!(WM_LBUTTONUP, WM_LBUTTONUP_ID);
    debug_assert_eq!(WM_LBUTTONDBLCLK, WM_LBUTTONDBLCLK_ID);
    debug_assert_eq!(WM_RBUTTONUP, WM_RBUTTONUP_ID);
    debug_assert_eq!(WM_CONTEXTMENU, WM_CONTEXTMENU_ID);

    let handle = match cc.window_handle() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!(
                "couldn't get the overlay's window handle; minimize will go to the taskbar: {err:?}"
            );
            return;
        }
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        log::warn!("overlay window handle isn't a Win32 handle; skipping the tray subclass");
        return;
    };
    let hwnd = HWND(win32.hwnd.get());

    if let Some(size) = default_inner_size {
        DEFAULT_INNER_SIZE_PT.store(
            pack_size(size.x, size.y),
            std::sync::atomic::Ordering::SeqCst,
        );
    } else {
        log::warn!(
            "no default inner size available; the tray's Reset Window will only recenter the overlay"
        );
    }

    // SAFETY: `hwnd` is winit's own handle for the window that owns this
    // `CreationContext`, alive and on the current thread; `tray_window_proc`
    // is a valid `SUBCLASSPROC` for the lifetime of the window (Windows
    // removes all subclasses automatically when the window is destroyed, so
    // no matching `RemoveWindowSubclass` is needed here).
    let installed = unsafe { SetWindowSubclass(hwnd, Some(tray_window_proc), TRAY_SUBCLASS_ID, 0) };
    if !installed.as_bool() {
        log::warn!("SetWindowSubclass failed; minimize will go to the taskbar instead of the tray");
        return;
    }

    // SAFETY: `RegisterWindowMessageW` only reads the 'static NUL-terminated
    // UTF-16 literal; registering the same name twice returns the same id, so
    // there is nothing to unregister.
    let taskbar_created = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
    if taskbar_created == 0 {
        log::warn!(
            "RegisterWindowMessageW(TaskbarCreated) failed; the tray icon won't come back if Explorer restarts"
        );
    }
    TASKBAR_CREATED_MESSAGE.store(taskbar_created, std::sync::atomic::Ordering::SeqCst);

    log::info!("installed the window-message subclass that minimizes to the notification area");
}

#[cfg(not(windows))]
pub fn install_tray(_cc: &eframe::CreationContext<'_>, _default_inner_size: Option<egui::Vec2>) {}

/// `SetWindowSubclass` callback installed by `install_tray`; see its doc
/// comment for the overall flow.
///
/// `WM_SIZE`/`SIZE_MINIMIZED` is forwarded to `DefSubclassProc` *before*
/// hiding, so winit finishes recording the minimize (and `ui.rs`'s
/// `track_window_position` sees the minimized flag and skips persisting
/// Windows' `-32000` parking coordinates) before the window disappears.
#[cfg(windows)]
unsafe extern "system" fn tray_window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _uidsubclass: usize,
    _dwrefdata: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{SIZE_MINIMIZED, WM_NCDESTROY, WM_SIZE};

    if msg == TRAY_CALLBACK_MESSAGE {
        // `add_tray_icon`/`install_tray` only ever call `Shell_NotifyIconW`
        // with `NIM_ADD` — `NIM_SETVERSION` is never sent, so the icon stays
        // on the legacy (pre-`NOTIFYICON_VERSION_4`) callback convention,
        // where `lParam` is just the raw mouse message (e.g.
        // `WM_LBUTTONUP`), not the mouse-message/uID pair packed into the
        // low/high words that `NOTIFYICON_VERSION_4` would use. The `&
        // 0xFFFF` mask below is therefore a no-op — mouse messages are all
        // well under `0x10000` — kept only because it's harmless and would
        // become load-bearing if this ever opts into version 4.
        match tray_action_for((lparam.0 as u32) & 0xFFFF) {
            Some(TrayAction::ShowMenu) => show_tray_menu(hwnd),
            Some(TrayAction::Restore) => restore_from_tray(hwnd),
            None => {}
        }
        return LRESULT(0);
    }

    // Checked before `WM_SIZE`/`WM_NCDESTROY` only for readability — the
    // shell allocates `TaskbarCreated` in the `0xC000..=0xFFFF` range, so it
    // can never collide with a system message.
    let taskbar_created = TASKBAR_CREATED_MESSAGE.load(std::sync::atomic::Ordering::SeqCst);
    if taskbar_created != 0
        && msg == taskbar_created
        && TRAY_ICON_ADDED.swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        // Explorer restarted and took every notification-area icon with it.
        // The flag was cleared above so `add_tray_icon` re-registers rather
        // than short-circuiting; if even that fails the overlay is un-hidden,
        // because a hidden window with no icon and no taskbar button is
        // unrecoverable.
        log::info!("the shell restarted; re-adding the overlay's notification-area icon");
        if !add_tray_icon(hwnd) {
            restore_from_tray(hwnd);
        }
    }

    if msg == WM_SIZE && (wparam.0 as u32) == SIZE_MINIMIZED {
        // SAFETY: forwarding this subclass procedure's own arguments
        // unchanged, exactly as the fall-through below does.
        let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        hide_to_tray(hwnd);
        return result;
    }

    if msg == WM_NCDESTROY {
        // Last message the window ever sees: the backstop that guarantees no
        // ghost icon is left in the notification area, whichever way the app
        // was closed.
        remove_tray_icon(hwnd);
    }

    // SAFETY: same as the `WM_SIZE` branch above.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Builds the `NOTIFYICONDATAW` describing this process's single tray icon.
/// Split out so `add_tray_icon` and `remove_tray_icon` can't disagree about
/// the (`hWnd`, `uID`) pair the shell identifies it by.
#[cfg(windows)]
fn tray_icon_data(
    hwnd: windows::Win32::Foundation::HWND,
) -> windows::Win32::UI::Shell::NOTIFYICONDATAW {
    use windows::Win32::UI::Shell::NOTIFYICONDATAW;

    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        ..Default::default()
    }
}

/// Resource id the application icon would be embedded under (`shinra-bpsr.rc`
/// / `build.rs`). No `.ico` is embedded today — the `.rc` carries only the
/// UAC manifest — so this lookup is expected to fail and fall back to the
/// stock application icon; it's tried first so that embedding one later is a
/// pure resource-file change with no code change here.
#[cfg(windows)]
const APP_ICON_RESOURCE_ID: u16 = 2;

/// The `HICON` for the tray entry: the executable's own embedded icon if
/// there is one, otherwise Windows' stock application icon. Both are
/// resource-backed shared icons owned by the module/system, so neither needs
/// `DestroyIcon`.
#[cfg(windows)]
fn tray_icon_handle() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{HICON, IDI_APPLICATION, LoadIconW};

    // SAFETY: `GetModuleHandleW(None)` asks for this process's own executable
    // module, which is loaded for the process's whole lifetime; the handle it
    // returns is borrowed, not owned, so it must not be freed.
    let module = unsafe { GetModuleHandleW(None) }.unwrap_or(HMODULE(0));
    if module.0 != 0 {
        // `MAKEINTRESOURCE`: a numeric resource id is passed as a `PCWSTR`
        // whose pointer value *is* the id, not a pointer to a string.
        let name = windows::core::PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16);
        // SAFETY: `module` is this process's own module handle and `name` is
        // a well-formed `MAKEINTRESOURCE` id; a missing resource is reported
        // as an error, not undefined behaviour.
        if let Ok(icon) = unsafe { LoadIconW(module, name) } {
            return icon;
        }
        log::info!(
            "no icon resource {APP_ICON_RESOURCE_ID} embedded in the executable; using the stock application icon in the notification area"
        );
    }

    // SAFETY: a null module handle with an `IDI_*` id is the documented way
    // to load a stock system icon.
    match unsafe { LoadIconW(HMODULE(0), IDI_APPLICATION) } {
        Ok(icon) => icon,
        Err(err) => {
            log::warn!("LoadIconW(IDI_APPLICATION) failed; the tray entry will be iconless: {err}");
            HICON(0)
        }
    }
}

/// Registers the notification-area icon. Returns whether the shell accepted
/// it — `hide_to_tray` refuses to hide the window when it didn't, since a
/// hidden window with no tray icon and no taskbar button is unreachable.
#[cfg(windows)]
fn add_tray_icon(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, Shell_NotifyIconW};

    if TRAY_ICON_ADDED.load(std::sync::atomic::Ordering::SeqCst) {
        return true;
    }

    let mut data = tray_icon_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = TRAY_CALLBACK_MESSAGE;
    data.hIcon = tray_icon_handle();
    // `szTip` is a fixed 128-`u16` buffer; `Default` already zeroed it, so
    // copying a shorter string in leaves it NUL-terminated.
    for (slot, unit) in data
        .szTip
        .iter_mut()
        .zip("shinra-bpsr".encode_utf16().chain(std::iter::once(0)))
    {
        *slot = unit;
    }

    // SAFETY: `data` is a correctly-sized, fully-initialized
    // `NOTIFYICONDATAW` that outlives this synchronous call; the shell
    // copies what it needs and retains no pointer into it.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    if !added.as_bool() {
        log::warn!("Shell_NotifyIconW(NIM_ADD) failed; leaving the overlay on the taskbar");
        return false;
    }
    TRAY_ICON_ADDED.store(true, std::sync::atomic::Ordering::SeqCst);
    true
}

/// Deletes the notification-area icon if one is registered. Idempotent, so
/// the restore paths and the `WM_NCDESTROY` backstop can all call it.
#[cfg(windows)]
fn remove_tray_icon(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::Shell::{NIM_DELETE, Shell_NotifyIconW};

    if !TRAY_ICON_ADDED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let data = tray_icon_data(hwnd);
    // SAFETY: same as `add_tray_icon` — `NIM_DELETE` only reads `cbSize`,
    // `hWnd` and `uID`, all of which `tray_icon_data` filled in.
    if !unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool() {
        log::warn!("Shell_NotifyIconW(NIM_DELETE) failed; a stale tray icon may remain");
    }
}

/// Hides the (already minimized) overlay so its taskbar button goes away,
/// leaving the notification-area icon as the only way back.
#[cfg(windows)]
fn hide_to_tray(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    if !add_tray_icon(hwnd) {
        return;
    }
    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread for the duration of this callback. `SW_HIDE`
    // sends `WM_WINDOWPOSCHANGING` with `SWP_NOMOVE | SWP_NOSIZE`, which
    // `window_proc`'s `already_pinned` check lets through untouched, so no
    // `app_driven_reposition` exemption is needed.
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    log::info!("overlay minimized to the notification area");
}

/// Un-hides and re-activates the overlay wherever it already is, and drops
/// the tray icon again.
#[cfg(windows)]
fn restore_from_tray(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    // `SW_SHOW` first, then `SW_RESTORE`: the window is both hidden *and*
    // minimized, and `SW_RESTORE` alone on a hidden window is documented to
    // restore it without necessarily bringing back the visibility the
    // `SW_HIDE` took away.
    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread. Neither call moves or resizes the window, so
    // neither can be vetoed by `window_proc`.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        ShowWindow(hwnd, SW_RESTORE);
        // Best-effort: Windows refuses foreground activation from a process
        // that doesn't currently own it, and there is nothing useful to do
        // about that beyond leaving the window visible.
        SetForegroundWindow(hwnd);
    }
    remove_tray_icon(hwnd);
    log::info!("overlay restored from the notification area");
}

/// The work area (`MONITORINFO.rcWork`) of the monitor `hwnd` sits on, or
/// the primary monitor's when it sits on none — which is exactly the case
/// "Reset Window" exists for, so `MONITOR_DEFAULTTOPRIMARY` (rather than
/// `..TONEAREST`) is the deliberate choice: a stranded overlay comes back on
/// the primary display, where the user is looking.
#[cfg(windows)]
fn monitor_work_area_for(hwnd: windows::Win32::Foundation::HWND) -> Option<Rect> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
    };

    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread; `info` is a correctly-sized out-param.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        log::warn!("GetMonitorInfoW failed; can't reset the overlay's position");
        return None;
    }
    Some(Rect {
        left: info.rcWork.left,
        top: info.rcWork.top,
        right: info.rcWork.right,
        bottom: info.rcWork.bottom,
    })
}

/// "Reset Window": un-hides the overlay and puts it back at `ui::viewport`'s
/// default inner size, centered on the monitor it's on (or the primary one).
///
/// The overlay is borderless (`ui::viewport`'s `with_decorations(false)`),
/// so its outer rect — which is what `SetWindowPos` takes — is its inner
/// rect, and the points-to-physical-pixels conversion is just the window's
/// DPI scale.
#[cfg(windows)]
fn reset_window(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SW_RESTORE, SW_SHOW, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
        ShowWindow, USER_DEFAULT_SCREEN_DPI,
    };

    // Un-hide first: a reset that left the window invisible would defeat the
    // whole point of the command.
    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        ShowWindow(hwnd, SW_RESTORE);
    }
    remove_tray_icon(hwnd);

    let Some(work_area) = monitor_work_area_for(hwnd) else {
        return;
    };

    // SAFETY: same as above. A `0` return means the call failed; fall back to
    // the unscaled 96 DPI baseline rather than collapsing the window to
    // nothing.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi == 0 {
        log::warn!("GetDpiForWindow failed; resetting the overlay at unscaled size");
        1.0
    } else {
        dpi as f32 / USER_DEFAULT_SCREEN_DPI as f32
    };

    let size = match unpack_size(DEFAULT_INNER_SIZE_PT.load(std::sync::atomic::Ordering::SeqCst)) {
        Some((width, height)) => ((width * scale) as i32, (height * scale) as i32),
        None => {
            // No default size to restore (see `install_tray`): recenter at
            // whatever size the window currently has, which is still the
            // recovery the user asked for minus the resize.
            let mut current = RECT::default();
            // SAFETY: same as above; `current` is a valid, correctly-sized
            // out-param.
            if !unsafe { GetWindowRect(hwnd, &mut current) }.as_bool() {
                log::warn!("GetWindowRect failed; can't reset the overlay's window");
                return;
            }
            (current.right - current.left, current.bottom - current.top)
        }
    };
    let target = default_window_rect(work_area, size);

    // Wrapped like every other direct `SetWindowPos` in this crate so the
    // Snap blocker can't mistake this for a shell-driven Snap — a default
    // size centered on a work area can land within `SNAP_SHAPE_TOLERANCE` of
    // a half/quarter shape.
    // SAFETY: same as above; `SWP_NOZORDER` means the second handle is
    // ignored.
    let moved = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            HWND(0),
            target.left,
            target.top,
            target.width(),
            target.height(),
            SWP_NOZORDER,
        )
    });
    if !moved.as_bool() {
        log::warn!("SetWindowPos failed while resetting the overlay's window");
        return;
    }
    // SAFETY: same as above; best-effort, see `restore_from_tray`.
    unsafe { SetForegroundWindow(hwnd) };
    log::info!(
        "reset the overlay to ({}, {}) {}x{}",
        target.left,
        target.top,
        target.width(),
        target.height()
    );
}

/// Pops the tray context menu at the cursor and runs whatever was picked.
///
/// The `SetForegroundWindow` before and the dummy `PostMessageW` after are
/// the long-standing Win32 workaround (KB135788) for a tray menu that
/// otherwise refuses to dismiss when the user clicks elsewhere: the menu's
/// modal loop only ends when its owner window is foreground, and the owner
/// only re-evaluates that on its next message.
#[cfg(windows)]
fn show_tray_menu(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, MF_STRING, PostMessageW,
        SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        TrackPopupMenu, WM_CLOSE, WM_NULL,
    };

    // SAFETY: `CreatePopupMenu` allocates a fresh, unowned menu; it is
    // destroyed on every exit path below.
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(err) => {
            log::warn!("CreatePopupMenu failed; the tray menu is unavailable: {err}");
            return;
        }
    };

    // SAFETY: `menu` is the handle just created; the `w!` literals are
    // 'static NUL-terminated UTF-16, which `AppendMenuW` copies.
    let appended = unsafe {
        AppendMenuW(
            menu,
            MF_STRING,
            TRAY_MENU_RESET_WINDOW as usize,
            windows::core::w!("Reset Window"),
        )
        .as_bool()
            && AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_EXIT as usize,
                windows::core::w!("Exit"),
            )
            .as_bool()
    };
    if !appended {
        log::warn!("AppendMenuW failed; the tray menu would be incomplete, so it isn't shown");
        // SAFETY: `menu` is a live, unowned menu handle not attached to any
        // window, so destroying it here is the only cleanup needed.
        unsafe { DestroyMenu(menu) };
        return;
    }

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid, correctly-sized out-param.
    if !unsafe { GetCursorPos(&mut cursor) }.as_bool() {
        log::warn!("GetCursorPos failed; showing the tray menu at the screen origin");
    }

    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread; `TPM_RETURNCMD` makes `TrackPopupMenu` return
    // the selected id (or 0) instead of posting a `WM_COMMAND`.
    let selected = unsafe {
        SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            None,
        );
        PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        DestroyMenu(menu);
        selected
    };

    match tray_command_for(selected.0 as u32) {
        Some(TrayCommand::ResetWindow) => reset_window(hwnd),
        Some(TrayCommand::Exit) => {
            // Routed through `WM_CLOSE` rather than `ExitProcess` so this is
            // the exact same shutdown the header's close button and alt-F4
            // take: winit reports it as a close request, `run_native`
            // returns, and `main.rs` stops capture, drains the pipeline and
            // flushes settings. The icon is dropped here as well as at
            // `WM_NCDESTROY` so it can't linger while shutdown runs.
            remove_tray_icon(hwnd);
            // SAFETY: `hwnd` is this subclass's window; posting (rather than
            // sending) `WM_CLOSE` lets the menu's modal loop unwind first.
            if !unsafe { PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) }.as_bool() {
                log::warn!("PostMessageW(WM_CLOSE) failed; the tray Exit command did nothing");
            }
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real GWL_STYLE bits, used to build realistic test fixtures without
    // depending on the (Windows-only) `windows` crate constants.
    const WS_POPUP: isize = 0x8000_0000;
    const WS_VISIBLE: isize = 0x1000_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;

    /// The exemption has to survive a whole manual gesture (many frames,
    /// many winit-issued `SetWindowPos` calls), so it is refcounted rather
    /// than a plain flag: a nested `app_driven_reposition` closure must not
    /// drop the exemption an outstanding gesture guard is holding open.
    #[test]
    fn the_app_driven_exemption_covers_both_guards_and_closures() {
        assert!(!app_driven_reposition_active());

        let gesture = begin_app_driven_reposition();
        assert!(app_driven_reposition_active());

        // A nested closure-scoped exemption comes and goes without
        // disturbing the gesture-scoped one.
        app_driven_reposition(|| assert!(app_driven_reposition_active()));
        assert!(app_driven_reposition_active());

        let nested = begin_app_driven_reposition();
        drop(nested);
        assert!(app_driven_reposition_active());

        drop(gesture);
        assert!(!app_driven_reposition_active());
    }

    #[test]
    fn clears_maximize_box() {
        let style = WS_POPUP | WS_VISIBLE | WS_THICKFRAME | WS_MAXIMIZEBOX_BIT;
        assert_eq!(without_maximize_box(style) & WS_MAXIMIZEBOX_BIT, 0);
    }

    #[test]
    fn preserves_other_style_bits() {
        let style = WS_POPUP | WS_VISIBLE | WS_THICKFRAME | WS_MAXIMIZEBOX_BIT;
        assert_eq!(
            without_maximize_box(style),
            WS_POPUP | WS_VISIBLE | WS_THICKFRAME
        );
    }

    #[test]
    fn unchanged_when_maximize_box_already_absent() {
        let style = WS_POPUP | WS_VISIBLE | WS_THICKFRAME;
        assert_eq!(without_maximize_box(style), style);
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn fully_on_screen_needs_no_correction() {
        let monitor = rect(0, 0, 1920, 1040);
        let window = rect(100, 100, 500, 400);
        assert_eq!(corrected_position(window, &[monitor]), None);
    }

    #[test]
    fn negative_x_on_secondary_monitor_left_of_primary_is_left_alone() {
        // Regression guard: a naive `[0, monitor_size]` clamp would wrongly
        // yank this window back onto the primary monitor, even though it's
        // already perfectly reachable where it is.
        let secondary = rect(-1920, 0, 0, 1040);
        let primary = rect(0, 0, 1920, 1040);
        let window = rect(-1800, 100, -1400, 400);
        assert_eq!(corrected_position(window, &[secondary, primary]), None);
    }

    #[test]
    fn position_on_a_disconnected_monitor_is_clamped_into_the_remaining_one() {
        let remaining = rect(0, 0, 1920, 1040);
        // Far outside every monitor: the monitor this was saved on is gone.
        let window = rect(5000, 5000, 5400, 5300);
        let Some((x, y)) = corrected_position(window, &[remaining]) else {
            panic!("expected a correction");
        };
        let (w, h) = (window.width(), window.height());
        assert!(x >= remaining.left && x + w <= remaining.right);
        assert!(y >= remaining.top && y + h <= remaining.bottom);
    }

    #[test]
    fn sliver_of_header_below_the_minimum_is_corrected() {
        let monitor = rect(0, 0, 1920, 1040);
        // Only 50px of header width would show — below MIN_VISIBLE_WIDTH.
        let window = rect(1870, 100, 2270, 400);
        assert!(corrected_position(window, &[monitor]).is_some());
    }

    #[test]
    fn header_visible_at_exactly_the_minimum_needs_no_correction() {
        let monitor = rect(0, 0, 1920, 1040);
        let window = rect(1920 - MIN_VISIBLE_WIDTH, 100, 2160, 400);
        assert_eq!(corrected_position(window, &[monitor]), None);
    }

    #[test]
    fn header_straddling_two_contiguous_monitors_needs_no_correction() {
        // Regression guard: the band is 160px wide (exactly
        // MIN_VISIBLE_WIDTH), fully on screen and perfectly draggable, but
        // split 80/80 across the seam between two edge-adjacent monitors.
        // Testing each monitor on its own would call that unreachable and
        // force-relocate a deliberately placed window.
        let left_monitor = rect(0, 0, 1920, 1040);
        let right_monitor = rect(1920, 0, 3840, 1040);
        let window = rect(1920 - 80, 100, 1920 + 80, 400);
        assert_eq!(
            corrected_position(window, &[left_monitor, right_monitor]),
            None
        );
    }

    #[test]
    fn header_spanning_a_gap_between_monitors_is_corrected() {
        // Same 80/80 split, but the monitors aren't contiguous — there's an
        // 80px hole between them that no display covers. The merge must not
        // bridge it.
        let left_monitor = rect(0, 0, 1920, 1040);
        let right_monitor = rect(2000, 0, 3920, 1040);
        let window = rect(1840, 100, 2080, 400);
        assert!(corrected_position(window, &[left_monitor, right_monitor]).is_some());
    }

    #[test]
    fn monitors_with_too_little_vertical_overlap_dont_merge() {
        // Each monitor covers half the band's width, so merging them would
        // clear MIN_VISIBLE_WIDTH — but neither overlaps the band
        // vertically by MIN_VISIBLE_HEIGHT (10px and 12px), so neither
        // contributes a grabbable strip and the band is unreachable.
        let top_monitor = rect(0, 0, 1920, 110);
        let bottom_monitor = rect(1920, 120, 3840, 1040);
        let window = rect(1840, 100, 2000, 400);
        assert!(corrected_position(window, &[top_monitor, bottom_monitor]).is_some());
    }

    #[test]
    fn empty_monitor_list_is_left_alone() {
        let window = rect(-32000, -32000, -31600, -31700);
        assert_eq!(corrected_position(window, &[]), None);
    }

    #[test]
    fn window_larger_than_the_work_area_is_pinned_to_top_left() {
        let monitor = rect(0, 0, 800, 600);
        // 1000x700 — bigger than the monitor on both axes.
        let window = rect(2000, 2000, 3000, 2700);
        assert_eq!(corrected_position(window, &[monitor]), Some((0, 0)));
    }

    #[test]
    fn windows_minimize_parking_spot_is_corrected() {
        // -32000, -32000 is where Windows parks a minimized window.
        // `ui::is_plausible_position` already guards the *save* side
        // against persisting that value; this is the *load*-side backstop
        // for whatever's already sitting in settings.json regardless of
        // how it got there (an older build, a hand-edit, a race) — the two
        // don't fight each other, they guard opposite ends of the same
        // value's lifecycle.
        let monitor = rect(0, 0, 1920, 1040);
        let window = rect(-32000, -32000, -31600, -31700);
        assert!(corrected_position(window, &[monitor]).is_some());
    }

    #[test]
    fn left_half_snap_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(0, 0, 960, 1040);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn right_half_snap_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(960, 0, 1920, 1040);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn top_half_snap_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(0, 0, 1920, 520);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn bottom_half_snap_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(0, 520, 1920, 1040);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn quarter_snap_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(960, 0, 1920, 520); // top-right quadrant
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn full_maximize_shape_is_rejected() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(0, 0, 1920, 1040);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn off_by_one_rounding_still_counts_as_snap_shaped() {
        // 1921 is odd, so a real half-split can't divide it evenly; Windows
        // rounds one side up by a pixel. The tolerance should absorb this.
        let monitor = rect(0, 0, 1921, 1040);
        let proposal = rect(0, 0, 961, 1040);
        assert!(is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn arbitrary_user_resize_is_not_snap_shaped() {
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(200, 150, 900, 700);
        assert!(!is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn user_drag_to_a_non_half_position_is_not_snap_shaped() {
        // Deliberately close to, but not matching, the left-half shape.
        let monitor = rect(0, 0, 1920, 1040);
        let proposal = rect(50, 50, 1010, 990);
        assert!(!is_snap_shaped(proposal, &[monitor]));
    }

    #[test]
    fn no_monitors_is_never_snap_shaped() {
        let proposal = rect(0, 0, 960, 1040);
        assert!(!is_snap_shaped(proposal, &[]));
    }

    #[test]
    fn snap_from_a_reachable_position_is_vetoed() {
        // The shipped behaviour: the overlay sits somewhere ordinary, a
        // Win+Arrow/shell snap proposes the left half, and it's refused.
        let monitor = rect(0, 0, 1920, 1040);
        let current = rect(400, 300, 800, 600);
        let proposal = rect(0, 0, 960, 1040);
        assert!(should_veto_snap(current, proposal, &[monitor]));
    }

    #[test]
    fn snap_shaped_rescue_of_a_stranded_window_is_not_vetoed() {
        // Regression guard for the veto's one damaging false positive: the
        // overlay was sized to roughly half the screen on a second monitor,
        // that monitor was unplugged, and Windows is repositioning it onto
        // the survivor at a rect that happens to match that monitor's left
        // half. Vetoing here would pin the window on the monitor that no
        // longer exists, with no header to grab.
        let survivor = rect(0, 0, 1920, 1040);
        let current = rect(-1920, 0, -960, 1040);
        let proposal = rect(0, 0, 960, 1040);
        // Shape alone would say "veto"; the reachability precondition is
        // the only thing standing between this proposal and a stranding.
        assert!(is_snap_shaped(proposal, &[survivor]));
        assert!(!should_veto_snap(current, proposal, &[survivor]));
    }

    #[test]
    fn an_ordinary_reposition_is_never_vetoed() {
        let monitor = rect(0, 0, 1920, 1040);
        let current = rect(400, 300, 800, 600);
        let proposal = rect(450, 320, 850, 620);
        assert!(!should_veto_snap(current, proposal, &[monitor]));
    }

    #[test]
    fn right_click_and_the_keyboard_menu_key_both_open_the_tray_menu() {
        assert_eq!(tray_action_for(WM_RBUTTONUP_ID), Some(TrayAction::ShowMenu));
        assert_eq!(
            tray_action_for(WM_CONTEXTMENU_ID),
            Some(TrayAction::ShowMenu)
        );
    }

    #[test]
    fn left_click_and_double_click_both_restore_the_overlay() {
        assert_eq!(tray_action_for(WM_LBUTTONUP_ID), Some(TrayAction::Restore));
        assert_eq!(
            tray_action_for(WM_LBUTTONDBLCLK_ID),
            Some(TrayAction::Restore)
        );
    }

    #[test]
    fn other_tray_mouse_messages_do_nothing() {
        // WM_MOUSEMOVE (0x0200) and WM_RBUTTONDOWN (0x0204): the shell sends
        // both to the callback, and acting on either would pop the menu on
        // hover or on button-*down*, leaving the matching up event to fall
        // through into the freshly-opened menu.
        assert_eq!(tray_action_for(0x0200), None);
        assert_eq!(tray_action_for(0x0204), None);
    }

    #[test]
    fn menu_ids_map_to_their_commands() {
        assert_eq!(
            tray_command_for(TRAY_MENU_RESET_WINDOW),
            Some(TrayCommand::ResetWindow)
        );
        assert_eq!(tray_command_for(TRAY_MENU_EXIT), Some(TrayCommand::Exit));
    }

    #[test]
    fn a_dismissed_tray_menu_selects_nothing() {
        // `TrackPopupMenu(TPM_RETURNCMD)` returns 0 when the user clicks away
        // instead of picking an item, which must not be mistaken for a
        // command id.
        assert_eq!(tray_command_for(0), None);
        assert_eq!(tray_command_for(99), None);
    }

    #[test]
    fn a_default_size_survives_the_atomic_round_trip() {
        let packed = pack_size(720.5, 460.25);
        assert_eq!(unpack_size(packed), Some((720.5, 460.25)));
    }

    #[test]
    fn the_unset_sentinel_and_degenerate_sizes_are_rejected() {
        // 0 is the "install_tray was never told a default size" sentinel; a
        // zero or negative extent would resize the window to nothing.
        assert_eq!(unpack_size(0), None);
        assert_eq!(unpack_size(pack_size(0.0, 400.0)), None);
        assert_eq!(unpack_size(pack_size(600.0, -1.0)), None);
        assert_eq!(unpack_size(pack_size(f32::NAN, 400.0)), None);
    }

    #[test]
    fn reset_centers_the_default_size_in_the_work_area() {
        let work_area = rect(0, 0, 1920, 1040);
        assert_eq!(
            default_window_rect(work_area, (720, 460)),
            rect(600, 290, 1320, 750)
        );
    }

    #[test]
    fn reset_centers_on_the_monitor_the_work_area_belongs_to() {
        // A monitor left of primary has negative coordinates; the reset must
        // land there, not at an absolute (0, 0)-relative position.
        let work_area = rect(-1920, 0, 0, 1040);
        let reset = default_window_rect(work_area, (720, 460));
        assert_eq!(reset, rect(-1320, 290, -600, 750));
    }

    #[test]
    fn reset_clamps_a_default_larger_than_the_work_area() {
        // A default sized for a big desktop, reset onto a small one: the
        // window must still fit entirely inside the work area rather than
        // hanging off it, since "I can't find my window" is what this
        // command exists to fix.
        let work_area = rect(0, 0, 800, 600);
        let reset = default_window_rect(work_area, (1200, 900));
        assert_eq!(reset, work_area);
    }

    #[test]
    fn a_reset_window_is_always_reachable() {
        // The whole point of the tray's Reset Window command: whatever it
        // produces has to pass the same reachability bar `corrected_position`
        // uses, on every layout.
        let monitors = [rect(-1920, 0, 0, 1040), rect(0, 0, 1920, 1040)];
        for work_area in monitors {
            let reset = default_window_rect(work_area, (720, 460));
            assert!(window_is_reachable(reset, &monitors));
            assert_eq!(corrected_position(reset, &monitors), None);
        }
    }

    #[test]
    fn a_stranded_window_is_never_vetoed_even_for_a_maximize_shape() {
        let monitor = rect(0, 0, 1920, 1040);
        // Parked at Windows' minimize coordinates: nothing grabbable.
        let current = rect(-32000, -32000, -31600, -31700);
        let proposal = rect(0, 0, 1920, 1040);
        assert!(!should_veto_snap(current, proposal, &[monitor]));
    }
}
