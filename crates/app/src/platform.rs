//! Windows-only startup tweaks that egui/winit don't expose a cross-platform
//! way to do.
//!
//! Issue #11: dragging the borderless overlay via
//! `ViewportCommand::StartDrag` (see `ui::draw_header`) enters winit's
//! native move loop, and Windows applies Aero Snap to that gesture the same
//! way it would a real title-bar drag. Snap behavior is driven by the
//! `WS_MAXIMIZEBOX` window style, not by whether the window has decorations,
//! so we clear that bit directly on the raw `HWND` right after the window is
//! created. `WS_THICKFRAME` — which `ui::draw_resize_handles` relies on for
//! edge-drag resizing via `BeginResize` — is untouched, so manual resize
//! keeps working.
//!
//! Not verified against a real Windows session: this workaround can only be
//! confirmed by actually dragging the overlay near a screen edge on
//! Windows, which isn't possible from this (Linux, cross-compiling)
//! environment.
//!
//! Issue #30: `clamp_window_to_visible_area` below is a second, unrelated
//! startup tweak that lives here for the same reason — it needs the raw
//! `HWND` and Win32 monitor APIs egui/eframe don't expose. See its own doc
//! comment and `corrected_position`'s for what it does and why. Likewise
//! not verified against a real Windows session; only the pure
//! `corrected_position` geometry is exercised by tests here.

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
    // SAFETY: same as above.
    let framechanged = unsafe {
        SetWindowPos(
            hwnd,
            HWND(0),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    };
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
/// *only* way the user can move it (see `ViewportCommand::StartDrag`
/// there). So "acceptable" isn't "fully on some monitor": a window sitting
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

    let band = Rect {
        left: window.left,
        top: window.top,
        right: window.right,
        bottom: window.top + HEADER_BAND_HEIGHT,
    };
    if band_is_reachable(band, monitors) {
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
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC};
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

    let mut monitors: Vec<Rect> = Vec::new();
    // SAFETY: `EnumDisplayMonitors` calls `monitor_enum_callback`
    // synchronously, on this thread, once per monitor, only for the
    // duration of this call — it doesn't retain the callback or the
    // `LPARAM` afterwards. The pointer handed to it here is `&mut
    // monitors`, a local that outlives the call, so the callback's
    // raw-pointer round-trip back to `&mut Vec<Rect>` is valid on every
    // invocation.
    let enumerated = unsafe {
        EnumDisplayMonitors(
            HDC(0),
            None,
            Some(monitor_enum_callback),
            LPARAM(std::ptr::addr_of_mut!(monitors) as isize),
        )
    };
    if !enumerated.as_bool() {
        // Keep going with whatever the callback did collect: a partial list
        // still beats no clamp at all, and the `is_empty` guard below covers
        // total failure.
        log::warn!("EnumDisplayMonitors failed; the monitor list may be partial");
    }
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
    // SAFETY: same as `GetWindowRect` above. `SWP_NOSIZE` means `cx`/`cy`
    // are ignored, and `SWP_NOZORDER` means the second handle is ignored.
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            HWND(0),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    // Real GWL_STYLE bits, used to build realistic test fixtures without
    // depending on the (Windows-only) `windows` crate constants.
    const WS_POPUP: isize = 0x8000_0000;
    const WS_VISIBLE: isize = 0x1000_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;

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
}
