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
//!
//! Issue #171: `http_get` is a fourth, unrelated addition living here for
//! the same reason as the rest — egui/eframe expose no HTTP client, so the
//! header dropdown's manual "Check for updates" reaches for WinHTTP
//! directly. See its doc comment for why WinHTTP over a new crate, and
//! `update_check`'s module doc comment for the pure request/decision logic
//! this feeds.

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

    // Since windows 0.62 every handle type is a newtype over `*mut c_void`
    // rather than over `isize`, so winit's raw `NonZeroIsize` has to be cast
    // to a pointer on the way in (and back to an integer on the way into
    // `OVERLAY_HWND` below).
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);
    // Cached so `force_frame_recompute` — called from `ui.rs`, which only
    // ever has an `egui::Context`, never a window handle — can find this
    // window later without resorting to `GetForegroundWindow` or
    // thread-window enumeration, either of which can resolve to the wrong
    // window. Stored unconditionally, even if the rest of this function
    // bails out below, since a partial Aero Snap workaround is still the
    // same live window `force_frame_recompute` needs to reach.
    OVERLAY_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
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

    // A `GWL_STYLE` change alone doesn't take effect — Win32 requires an
    // explicit `SWP_FRAMECHANGED` `SetWindowPos` before the window manager
    // re-reads the style bits. That, and *only* that, is what this call is
    // for: it commits the `WS_MAXIMIZEBOX` clear above, which is what
    // actually disables Aero Snap on drag.
    //
    // NOT the transparency fix, despite what this comment used to claim.
    // Issue #89 measured it directly: issuing this exact `SetWindowPos` on
    // the live window changes zero pixels, as do `RedrawWindow`, hide/show,
    // minimize/restore and re-applying `DwmEnableBlurBehindWindow`. The
    // startup gray is DWM compositing wgpu's premultiplied-alpha swapchain
    // over the opaque, white-initialized GDI redirection surface it
    // allocated at `CreateWindowEx` — a surface no post-creation call can
    // remove, which is why the real fix (`WS_EX_NOREDIRECTIONBITMAP` plus
    // DirectComposition presentation) lives in `main.rs` and the vendored
    // `third_party/egui-winit`, and why this call still has to stay: the
    // Aero Snap workaround above is inert without it.
    //
    // `SWP_NOMOVE | SWP_NOSIZE` means this call only asks for the
    // frame-change side effect, not an actual move/resize, so the
    // position/size arguments are ignored. `SWP_NOZORDER` likewise makes the
    // `hWndInsertAfter` argument unused, which is what `None` spells now that
    // windows models that parameter as an `Option<HWND>` rather than as the
    // null handle `HWND(0)` used to stand in for.
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
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    });
    if let Err(err) = framechanged {
        log::warn!(
            "SetWindowPos(SWP_FRAMECHANGED) failed ({err}); the WS_MAXIMIZEBOX clear may not have taken effect and Aero Snap may still trigger on drag"
        );
    }

    log::info!("cleared WS_MAXIMIZEBOX to disable Aero Snap on drag");
}

#[cfg(not(windows))]
pub fn disable_aero_snap(_cc: &eframe::CreationContext<'_>) {}

/// The overlay's `HWND`, cached by `disable_aero_snap` at startup so
/// `force_frame_recompute` below can find the window later. `0` means "not
/// yet cached" (or `disable_aero_snap` never resolved a handle), which
/// `force_frame_recompute` treats as a safe no-op.
///
/// A cached value rather than a fresh lookup at call time deliberately:
/// `force_frame_recompute` runs from `ui.rs` call sites that only have an
/// `egui::Context`, and both `GetForegroundWindow` and enumerating this
/// thread's windows risk resolving to a *different* window (another
/// always-on-top overlay, or whatever has focus mid-gesture) instead of this
/// one.
#[cfg(windows)]
static OVERLAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Issue #74: asks Windows to recompute the overlay's frame after an
/// app-driven resize, via the same `SWP_FRAMECHANGED` `SetWindowPos`
/// `disable_aero_snap` issues once at startup.
///
/// NOT the transparency fix. Issue #74 added this on the theory that
/// `SWP_FRAMECHANGED` is what makes DWM honor per-pixel alpha, and that the
/// header stayed opaque gray because nothing re-issued it after a resize.
/// Issue #89 disproved that: this call changes zero pixels on the live
/// window, and the startup gray was DWM compositing wgpu's
/// premultiplied-alpha swapchain over the opaque GDI redirection surface it
/// allocated at `CreateWindowEx` — see `disable_aero_snap` above and the
/// real fix in `main.rs` / `third_party/egui-winit`. What it *does* do is
/// force a frame/non-client recompute, which remains the correct thing to
/// do after a resize the window manager didn't drive itself (since issue
/// #11's rework, `ui.rs`'s manual drag-resize goes through
/// `ViewportCommand::OuterPosition`/`InnerSize`/`MinInnerSize`, never a raw
/// `SetWindowPos`, so Win32 is never told the frame moved).
///
/// Kept, rather than deleted along with the theory, because it is cheap,
/// side-effect-free under the no-op flags below, and the honest state of
/// knowledge is "harmless and probably right", not "proven useless". It
/// uses the `HWND` `disable_aero_snap` cached in `OVERLAY_HWND` above
/// rather than a handle of its own — the `ui.rs` call sites don't have one.
///
/// `SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE` means this
/// changes nothing but the frame, exactly like `disable_aero_snap`'s own
/// call. Wrapped in `app_driven_reposition` for the same reason every direct
/// `SetWindowPos` in this crate is: the no-op flags make it an unlikely veto
/// target, but the invariant is "every call is wrapped", not "only the ones
/// that could plausibly be vetoed".
///
/// Callers must fire this once per completed resize (gesture end), never
/// every frame of an in-progress one — the
/// whole issue #11/#68 rework exists to keep manual resize out of native
/// Win32 loops, and a `SetWindowPos` on every frame of an active drag risks
/// feeding back into egui's own viewport sizing and reproducing #68's resize
/// runaway.
#[cfg(windows)]
pub fn force_frame_recompute() {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };

    let raw = OVERLAY_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if raw == 0 {
        // Not yet cached — e.g. called before `disable_aero_snap` ran, which
        // shouldn't happen but costs nothing to guard against instead of
        // handing `SetWindowPos` a null handle.
        return;
    }
    // `OVERLAY_HWND` stores the handle as a bare integer (no atomic can hold
    // a `*mut c_void`), so it is cast back to a pointer here — the inverse of
    // the store in `disable_aero_snap`.
    let hwnd = HWND(raw as *mut std::ffi::c_void);

    // SAFETY: `hwnd` was cached from winit's own handle for this window in
    // `disable_aero_snap`; the window lives for the whole process, so the
    // handle is still valid. `SWP_NOMOVE | SWP_NOSIZE` means the
    // position/size arguments below are ignored.
    let framechanged = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    });
    if let Err(err) = framechanged {
        log::warn!(
            "SetWindowPos(SWP_FRAMECHANGED) failed after an app-driven resize ({err}); the window's frame may be stale"
        );
    }
}

#[cfg(not(windows))]
pub fn force_frame_recompute() {}

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

    /// Whether `point` (physical pixels, same coordinate space as the rect
    /// itself) falls inside — left/top inclusive, right/bottom exclusive,
    /// matching this type's own doc comment and Win32's `RECT` convention.
    /// Used by `click_through_hit_test` to decide whether a `WM_NCHITTEST`
    /// point lands on the click-through button's published hit box (issue
    /// #167 rehash).
    fn contains(self, point: (i32, i32)) -> bool {
        point.0 >= self.left && point.0 < self.right && point.1 >= self.top && point.1 < self.bottom
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
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

    let mut win_rect = RECT::default();
    // SAFETY: `hwnd` is winit's own handle for the window that owns this
    // `CreationContext`, alive and on the current thread; `win_rect` is a
    // valid, correctly-sized out-param for the duration of this call.
    if let Err(err) = unsafe { GetWindowRect(hwnd, &mut win_rect) } {
        log::warn!("GetWindowRect failed ({err}); skipping the position clamp");
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
    // are ignored, and `SWP_NOZORDER` means the `hWndInsertAfter` argument is
    // unused — hence `None`.
    let moved = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    });
    if let Err(err) = moved {
        log::warn!("SetWindowPos failed while correcting the overlay's position: {err}");
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
) -> windows::core::BOOL {
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
    use windows::Win32::Graphics::Gdi::EnumDisplayMonitors;

    let mut monitors: Vec<Rect> = Vec::new();
    // SAFETY: see `monitor_enum_callback`'s doc comment above — same
    // synchronous, single-threaded, non-retaining call. The `None` `hdc` is
    // what the old null `HDC(0)` meant: enumerate over the whole virtual
    // screen rather than clipping to some device context.
    let enumerated = unsafe {
        EnumDisplayMonitors(
            None,
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

/// Largest window extent, in physical pixels, the overlay can be allowed to
/// reach on either axis.
///
/// This is not a taste decision, it is the renderer's hard limit. egui-wgpu
/// asks for a device with `max_texture_dimension_2d: 8192` (see
/// `egui_wgpu::WgpuSetupCreateNew::default`'s `device_descriptor`, which
/// `main.rs`'s issue #89 DirectComposition setup leaves untouched), and
/// `Surface::configure` validates the swapchain against exactly that. wgpu
/// handles validation errors as fatal by default, so a window even one pixel
/// past this is not a rendering glitch — it is an immediate process-wide
/// panic on the next resize:
///
/// ```text
/// In Surface::configure
///   `Surface` width and height must be within the maximum supported texture
///   size. Requested was (460, 32767), maximum extent for either dimension is 8192.
/// ```
///
/// 8192 comfortably clears any real display (an 8K panel is 7680 wide), so
/// clamping here costs nothing a user could ever want.
#[cfg(any(windows, test))]
const MAX_WINDOW_EXTENT_PX: i32 = 8192;

/// The size a `WM_WINDOWPOSCHANGING` proposal has to be countermanded to, or
/// `None` when it already fits inside `MAX_WINDOW_EXTENT_PX` and should be
/// passed through untouched.
///
/// `window_proc` applies this to every proposal that isn't `SWP_NOSIZE`,
/// whoever raised it — the overlay's own resize gesture, winit, the shell, or
/// an external `MoveWindow`/`SetWindowPos` from another process (which is how
/// the 32767 in the doc comment above was actually reached: Win32 clamps a
/// window's width/height to `SHRT_MAX`, so an oversize external request comes
/// back through `WM_SIZE` as exactly 32767 and eframe feeds that straight into
/// `Surface::configure`). `WM_WINDOWPOSCHANGING` is the one seam this process
/// owns that every one of those paths passes through.
///
/// Pure integer geometry, no windows-crate types, so it is unit-testable off
/// Windows like the rest of this module's helpers.
#[cfg(any(windows, test))]
fn clamped_window_extent(cx: i32, cy: i32) -> Option<(i32, i32)> {
    let clamped = (cx.min(MAX_WINDOW_EXTENT_PX), cy.min(MAX_WINDOW_EXTENT_PX));
    (clamped != (cx, cy)).then_some(clamped)
}

/// Human-readable names for the `SWP_*` bits set in a `WM_WINDOWPOSCHANGING`
/// proposal's `flags` (issue #119), e.g. `NOMOVE|NOZORDER` instead of a raw
/// integer nobody can read at a glance. This is the single most load-bearing
/// piece of the enriched diagnostic: a proposal carrying `NOMOVE` (a pure
/// resize) tells a very different story from one with neither `NOMOVE` nor
/// `NOSIZE` set (a full move+resize, the shape an external `MoveWindow`/
/// `SetWindowPos` would send). `NOOWNERZORDER` and `NOREPOSITION` share bit
/// `0x200`, so only the former is named. Any bit this table doesn't
/// recognize is appended as a raw hex remainder rather than silently
/// dropped. Pure `u32` in and `String` out — no windows-crate types — so
/// this is unit-testable off Windows.
#[cfg(any(windows, test))]
fn decode_swp_flags(flags: u32) -> String {
    const NAMED: &[(u32, &str)] = &[
        (0x0001, "NOSIZE"),
        (0x0002, "NOMOVE"),
        (0x0004, "NOZORDER"),
        (0x0008, "NOREDRAW"),
        (0x0010, "NOACTIVATE"),
        (0x0020, "FRAMECHANGED"),
        (0x0040, "SHOWWINDOW"),
        (0x0080, "HIDEWINDOW"),
        (0x0100, "NOCOPYBITS"),
        (0x0200, "NOOWNERZORDER"),
        (0x0400, "NOSENDCHANGING"),
        (0x2000, "DEFERERASE"),
        (0x4000, "ASYNCWINDOWPOS"),
    ];
    let names: Vec<&str> = NAMED
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    let recognized_mask = NAMED.iter().fold(0u32, |acc, (bit, _)| acc | bit);
    let remainder = flags & !recognized_mask;
    let mut out = if names.is_empty() {
        "NONE".to_string()
    } else {
        names.join("|")
    };
    if remainder != 0 {
        out.push_str(&format!("|0x{remainder:04x}"));
    }
    out
}

/// The raw `x`/`y`/`cx`/`cy` from a `WM_WINDOWPOSCHANGING` proposal, bundled
/// only to keep [`oversize_proposal_log`]'s argument count under clippy's
/// too-many-arguments threshold — not a general-purpose window-pos type, so
/// it lives right next to its one caller rather than near `Rect`.
#[cfg(any(windows, test))]
struct ProposedWindowPos {
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
}

/// Builds the diagnostic line for an oversize `WM_WINDOWPOSCHANGING`
/// proposal (issue #119). The raw `warn!` this replaced logged only the
/// proposed `cx`x`cy` — exactly the information the #119 crash already
/// proved insufficient (`460x32767` says nothing about who asked or in what
/// state). `window_proc` gathers the impure Win32 state (current rect, DPI,
/// gesture flag) and hands it here so the formatting itself stays
/// unit-testable without a window; see `boss_transition_log`/
/// `scene_transition_log` in `meter::encounter` for the same pure-formatter
/// register this follows.
#[cfg(any(windows, test))]
fn oversize_proposal_log(
    proposed: ProposedWindowPos,
    clamped: (i32, i32),
    flags: u32,
    current: Option<Rect>,
    dpi: Option<u32>,
    gesture_active: bool,
) -> String {
    let ProposedWindowPos { x, y, cx, cy } = proposed;
    let (clamped_cx, clamped_cy) = clamped;
    let current = current.map_or_else(
        || "<unavailable>".to_string(),
        |r| format!("{}x{} at ({}, {})", r.width(), r.height(), r.left, r.top),
    );
    let dpi = dpi.map_or_else(|| "<unavailable>".to_string(), |d| d.to_string());
    format!(
        "clamping an oversize window proposal: proposed=(x={x}, y={y}, {cx}x{cy}) \
         clamped_to={clamped_cx}x{clamped_cy} flags={} current_window={current} dpi={dpi} \
         app_driven_reposition_active={gesture_active}; the renderer cannot present a surface \
         larger than {MAX_WINDOW_EXTENT_PX}px",
        decode_swp_flags(flags)
    )
}

/// Last oversize `WM_WINDOWPOSCHANGING` proposal `window_proc` clamped this
/// session, paired with the `Instant` it was stored at — `None` until the
/// first one happens. `install_panic_hook` in `logging.rs` reads this
/// through [`last_oversize_proposal`] so a `Surface::configure` panic can
/// still show the context that led to it: the clamp is not the only path to
/// an oversize surface, so the panic can still happen even after a proposal
/// was countermanded (issue #119). The `Instant` is what lets a reader tell
/// a clamp from moments ago apart from one from days into a long-running
/// session — this record is never cleared, so without an age a stale entry
/// would read as if it just happened.
///
/// `#[cfg(any(windows, test))]` rather than plain `#[cfg(windows)]` so the
/// store/read round trip and the poison-recovery path both have unit
/// coverage on the ubuntu-only CI host — see the `tests` module below.
#[cfg(any(windows, test))]
static LAST_OVERSIZE_PROPOSAL: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, std::time::Instant)>>,
> = std::sync::OnceLock::new();

/// Records `message` as the most recent oversize proposal, timestamped
/// `Instant::now()`. Called from `window_proc`, which never has a reason to
/// fail loudly here — losing this record would only make the *next*
/// panic's diagnostics worse, not this call's own correctness — so a
/// poisoned lock (some other panic already in flight) is recovered from
/// rather than propagated.
#[cfg(any(windows, test))]
fn store_last_oversize_proposal(message: String) {
    let lock = LAST_OVERSIZE_PROPOSAL.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some((message, std::time::Instant::now()));
}

/// Formats a duration as a short human-readable "ago" age, coarsest unit
/// first (e.g. `"3d 4h"`, `"12m 5s"`), so a reader can judge at a glance
/// whether a proposal is recent enough to be relevant to whatever panic it
/// is being read alongside. Deliberately just two components — this is a
/// diagnostic aid, not a precise clock.
#[cfg(any(windows, test))]
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// The last oversize proposal [`store_last_oversize_proposal`] recorded, if
/// any, formatted and ready to log with its age prefixed — e.g. `"last
/// oversize window proposal (3d 4h ago): clamping an oversize window
/// proposal: ..."`. `logging.rs`'s panic hook calls this; see
/// `LAST_OVERSIZE_PROPOSAL`'s doc comment for why it exists. The age is
/// computed fresh on every call (not at store time), since the gap that
/// matters is between the clamp and *this* read, not the clamp and process
/// start.
///
/// Takes the lock without blocking, because a panic hook must never
/// deadlock. `store_last_oversize_proposal` holds this lock across nothing
/// but an assignment, but "nothing but an assignment" is not "nothing": a
/// panic raised on this thread while that guard is alive would re-enter
/// here, and `Mutex` is not reentrant, so a blocking `lock()` would hang the
/// panicking process instead of reporting it. Failing to read the record
/// costs one supplementary log line; hanging costs the whole panic report,
/// which is the very thing this exists to improve. A poisoned lock is
/// recovered from for the same reason `store_last_oversize_proposal`
/// recovers from one — the record is diagnostic, and a panic already in
/// flight elsewhere is exactly when it is most worth reading.
#[cfg(any(windows, test))]
pub fn last_oversize_proposal() -> Option<String> {
    let lock = LAST_OVERSIZE_PROPOSAL.get_or_init(|| std::sync::Mutex::new(None));
    let guard = match lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    guard.as_ref().map(|(message, stored_at)| {
        format!(
            "last oversize window proposal ({} ago): {message}",
            format_elapsed(stored_at.elapsed())
        )
    })
}

/// Stub for a non-Windows, non-test host build, which never installs
/// `window_proc` and so never has a proposal to report; keeps `logging.rs`
/// free of a hard Windows-only dependency. `cfg(not(any(windows, test)))`
/// rather than `cfg(not(windows))` so this doesn't collide with the real
/// definition above, which is also test-enabled on every host.
#[cfg(not(any(windows, test)))]
pub fn last_oversize_proposal() -> Option<String> {
    None
}

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

/// What a `WM_NCHITTEST` point should resolve to (issue #167 rehash).
///
/// Replaces the shipped `Ctrl+Alt+P` hotkey escape hatch for click-through,
/// which turned out not to work: egui only sees key events while the
/// overlay holds keyboard focus, and the entire point of click-through is
/// to click the game *behind* the overlay, which takes focus away from the
/// window the hotkey needed it on. `window_proc`'s `WM_NCHITTEST` branch
/// intercepts the hit test directly instead, so the decision is made by
/// Windows' own mouse routing rather than by whichever window currently
/// has focus.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickThroughHitTest {
    /// Click-through is off: don't touch the hit test at all — let
    /// `DefSubclassProc` (and winit's own handling underneath it) decide,
    /// exactly as if this subclass never intercepted `WM_NCHITTEST`.
    PassThrough,
    /// Click-through is on, but the point is inside the click-through
    /// button's published hit box (or no hit box has been published yet —
    /// see `click_through_hit_test`'s doc comment): treat it as ordinary
    /// client-area interaction, so the button stays clickable.
    Client,
    /// Click-through is on and the point is outside the button: report
    /// `HTTRANSPARENT` so Windows re-does the hit test against whatever
    /// sits behind this window — the game.
    Transparent,
}

/// The pure decision behind `window_proc`'s `WM_NCHITTEST` handling (issue
/// #167 rehash). `point` and `button_rect` are both client-area physical
/// pixels — the space `ScreenToClient` converts a real `WM_NCHITTEST`
/// point into, and the space `set_click_through_button_rect` stores its
/// rect in.
///
/// The click-through button is deliberately excluded from passthrough by
/// this carve-out — it is never itself click-through-able — so it stays
/// reachable with the mouse in every state, unlike the whole-window
/// `ViewportCommand::MousePassthrough` this replaces (which made the
/// button that turns click-through off unclickable the moment click-through
/// turned on). The tray menu's "Turn off click-through" entry
/// (`install_tray`) is kept anyway, as a belt-and-braces fallback for the
/// case this carve-out ever fails or the button ends up off-screen.
///
/// `button_rect` being `None` — rather than a real rect that simply
/// doesn't contain `point` — means `toggle_cluster` hasn't published one
/// yet: the window has only just been created, before its first frame.
/// Resolving that to `Client` rather than `Transparent` is the deliberate
/// safe choice — a `None` that resolved to `Transparent` would make the
/// *entire* window click-through with no carve-out at all for however many
/// frames that startup gap lasts, which is exactly the kind of stranding
/// this mechanism exists to prevent.
#[cfg(any(windows, test))]
fn click_through_hit_test(
    point: (i32, i32),
    button_rect: Option<Rect>,
    click_through_on: bool,
) -> ClickThroughHitTest {
    if !click_through_on {
        return ClickThroughHitTest::PassThrough;
    }
    match button_rect {
        None => ClickThroughHitTest::Client,
        Some(rect) if rect.contains(point) => ClickThroughHitTest::Client,
        Some(_) => ClickThroughHitTest::Transparent,
    }
}

/// Real Win32 `WM_NCHITTEST` result codes (issue #232), mirrored as plain
/// literals rather than imported from the `windows` crate — see
/// `resize_lock_hit_test`'s doc comment for why. `window_proc`
/// `debug_assert`s each of these against the real
/// `windows::Win32::UI::WindowsAndMessaging` constants once, the same
/// belt-and-braces `disable_aero_snap` already does for
/// `WS_MAXIMIZEBOX_BIT`.
#[cfg(any(windows, test))]
const HT_CLIENT: i32 = 1;
#[cfg(any(windows, test))]
const HT_LEFT: i32 = 10;
#[cfg(any(windows, test))]
const HT_RIGHT: i32 = 11;
#[cfg(any(windows, test))]
const HT_TOP: i32 = 12;
#[cfg(any(windows, test))]
const HT_TOP_LEFT: i32 = 13;
#[cfg(any(windows, test))]
const HT_TOP_RIGHT: i32 = 14;
#[cfg(any(windows, test))]
const HT_BOTTOM: i32 = 15;
#[cfg(any(windows, test))]
const HT_BOTTOM_LEFT: i32 = 16;
#[cfg(any(windows, test))]
const HT_BOTTOM_RIGHT: i32 = 17;

/// The eight `WM_NCHITTEST` codes that denote one of the resize-border
/// regions — the only codes `resize_lock_hit_test` ever overrides.
#[cfg(any(windows, test))]
const RESIZE_BORDER_HIT_TESTS: [i32; 8] = [
    HT_LEFT,
    HT_RIGHT,
    HT_TOP,
    HT_BOTTOM,
    HT_TOP_LEFT,
    HT_TOP_RIGHT,
    HT_BOTTOM_LEFT,
    HT_BOTTOM_RIGHT,
];

/// Issue #232: overrides a resize-border `WM_NCHITTEST` result to plain
/// client area while the overlay is pinned, so Windows' own edge-drag
/// resize — driven by `WS_THICKFRAME`, entirely independent of `ui.rs`'s
/// manual `draw_resize_handles` gesture — can't resize a locked window
/// either. Pinning already locked the manual gesture (`ui::
/// resize_locked_by_pin`); without this, that lock was only half real,
/// since the invisible native resize border sitting right underneath the
/// same edges was never touched by it.
///
/// `default_hit_test` is whatever `DefSubclassProc` would have answered —
/// `window_proc`'s `WM_NCHITTEST` branch computes it first (by forwarding
/// the message down before deciding what to return) and passes it through
/// here rather than re-deriving the point/border math itself, so this
/// stays a plain lookup with nothing platform-specific to test. Anything
/// other than one of the eight resize-border codes — `HT_CLIENT`,
/// `HTCAPTION`, `HTNOWHERE`, etc — passes through unchanged even while
/// locked, since none of those grow or shrink the window.
///
/// Kept off the real `windows` crate constants — plain literals instead —
/// so this stays unit-testable on this (Linux, cross-compiling) dev host,
/// where `windows` is a Windows-only dependency
/// (`[target.'cfg(windows)'.dependencies]`) unavailable to a plain `cargo
/// test`.
#[cfg(any(windows, test))]
fn resize_lock_hit_test(default_hit_test: i32, resize_locked: bool) -> i32 {
    if resize_locked && RESIZE_BORDER_HIT_TESTS.contains(&default_hit_test) {
        HT_CLIENT
    } else {
        default_hit_test
    }
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
/// - `WM_NCHITTEST` (issue #167 rehash): while `Settings::click_through`
///   is on, carves the click-through button's published hit box
///   (`set_click_through_button_rect`) out of an otherwise fully
///   click-through window — `click_through_hit_test` is the pure decision,
///   see its doc comment for why this replaced `ViewportCommand::
///   MousePassthrough` and the `Ctrl+Alt+P` hotkey that came with it. Issue
///   #232 added a second, independent job to the same branch: while
///   `Settings::always_on_top` (the pin) is on, every resize-border result
///   `DefSubclassProc` would otherwise return is overridden to plain
///   client area (`resize_lock_hit_test`), so the native `WS_THICKFRAME`
///   edge-drag resize can't move a locked window's size either.
///
/// Neither the Snap handlers nor the click-through/resize-lock one touch
/// focus or z-order, so always-on-top and the game keeping focus are
/// unaffected; every other message falls through to `DefSubclassProc`
/// unchanged.
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
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

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
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT, HTTOP,
        HTTOPLEFT, HTTOPRIGHT, HTTRANSPARENT, SC_MAXIMIZE, SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS,
        WM_DPICHANGED, WM_NCHITTEST, WM_SYSCOMMAND, WM_WINDOWPOSCHANGING,
    };

    if msg == WM_WINDOWPOSCHANGING {
        // SAFETY: for `WM_WINDOWPOSCHANGING`, `lparam` is a valid pointer
        // to a `WINDOWPOS` the sender owns for the duration of this call;
        // mutating it in place before forwarding to `DefSubclassProc` is
        // exactly how a handler is meant to countermand a proposed
        // move/resize.
        let windowpos = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };

        // Both branches below need the window's current on-screen rect,
        // and neither of them changes it before the other runs: mutating
        // `windowpos.cx`/`cy` in the oversize branch only edits the
        // *proposed* `WINDOWPOS` in place, and nothing applies that
        // proposal to the real window until this callback returns. So one
        // `GetWindowRect` call, taken up front, is exactly as current for
        // the snap-veto check below as a second call would be.
        // SAFETY: `hwnd` is the window this subclass is installed on,
        // alive and on the current thread for the duration of this
        // callback; `current_rect` is a valid, correctly-sized out-param.
        let mut current_rect = RECT::default();
        let current_rect = unsafe { GetWindowRect(hwnd, &mut current_rect) }
            .is_ok()
            .then_some(Rect {
                left: current_rect.left,
                top: current_rect.top,
                right: current_rect.right,
                bottom: current_rect.bottom,
            });

        // Cap the proposed size before anything else looks at
        // it. Unlike the Snap veto below this runs for *every* proposal,
        // app-driven ones included, because the renderer's limit is not a
        // policy about who asked — a swapchain wider or taller than
        // `MAX_WINDOW_EXTENT_PX` is fatal whatever moved the window.
        if (windowpos.flags & SWP_NOSIZE).0 == 0
            && let Some((cx, cy)) = clamped_window_extent(windowpos.cx, windowpos.cy)
        {
            // Issue #119: this proposal is the crash's only lead, so gather
            // everything the next occurrence might need before it's gone.
            // SAFETY: `hwnd` is the window this subclass is installed on,
            // alive and on the current thread for the duration of this
            // callback. A `0` return means the call failed, per
            // `GetDpiForWindow`'s documented failure contract (also relied
            // on by `reset_window`).
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            let dpi = (dpi != 0).then_some(dpi);

            let message = oversize_proposal_log(
                ProposedWindowPos {
                    x: windowpos.x,
                    y: windowpos.y,
                    cx: windowpos.cx,
                    cy: windowpos.cy,
                },
                (cx, cy),
                windowpos.flags.0,
                current_rect,
                dpi,
                app_driven_reposition_active(),
            );
            log::warn!("{message}");
            store_last_oversize_proposal(message);
            windowpos.cx = cx;
            windowpos.cy = cy;
        }

        // If the proposal is already pinned on move and/or size, it isn't
        // a full move+resize and so can't be a snap shape (Snap always
        // both moves and resizes) — nothing to veto.
        let already_pinned = (windowpos.flags & (SWP_NOMOVE | SWP_NOSIZE)).0 != 0;
        if !already_pinned && !app_driven_reposition_active() {
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
            match current_rect {
                None => {
                    // Without the current rect there's no way to tell a
                    // veto from a stranding, and letting a Snap through is
                    // the cheaper mistake of the two.
                    log::warn!(
                        "GetWindowRect failed in the Snap blocker; letting this reposition through"
                    );
                }
                Some(current) => {
                    if should_veto_snap(current, proposed, &enumerate_monitor_work_areas()) {
                        windowpos.flags = windowpos.flags | SWP_NOMOVE | SWP_NOSIZE;
                    }
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
    } else if msg == WM_DPICHANGED {
        // Issue #119: nothing in this crate handles `WM_DPICHANGED` today,
        // so a DPI-change-driven resize is currently invisible in the
        // logs. This is a diagnostic tap only — it neither reads the
        // suggested rect for any decision nor stops the message from
        // reaching winit's own handling below.
        let new_dpi = (wparam.0 & 0xFFFF) as u32;
        // SAFETY: for `WM_DPICHANGED`, `lparam` is a valid pointer to a
        // `RECT` suggesting the window's new bounds at the new DPI, owned
        // by the sender for the duration of this call; this only reads it.
        let suggested = unsafe { &*(lparam.0 as *const RECT) };
        log::debug!(
            "WM_DPICHANGED: new dpi={new_dpi} suggested_rect=({}, {}, {}, {}) [{}x{}]",
            suggested.left,
            suggested.top,
            suggested.right,
            suggested.bottom,
            suggested.right - suggested.left,
            suggested.bottom - suggested.top
        );
    } else if msg == WM_NCHITTEST
        && (CLICK_THROUGH_ENABLED.load(std::sync::atomic::Ordering::SeqCst)
            || RESIZE_LOCKED.load(std::sync::atomic::Ordering::SeqCst))
    {
        // `WM_NCHITTEST`'s `lParam` packs the point as two signed 16-bit
        // words in *screen* coordinates (never a pointer, unlike the
        // `WINDOWPOS`/`RECT` messages above) — sign-extended through `i16`
        // first, since a point on a monitor above/left of the primary one
        // (see `Rect`'s doc comment) is legitimately negative.
        let raw = lparam.0 as u32;
        let mut point = POINT {
            x: (raw & 0xFFFF) as u16 as i16 as i32,
            y: ((raw >> 16) & 0xFFFF) as u16 as i16 as i32,
        };
        // SAFETY: `hwnd` is the window this subclass is installed on,
        // alive and on the current thread; `point` is a valid in/out
        // pointer `ScreenToClient` overwrites with the client-relative
        // equivalent in place.
        let converted = unsafe { ScreenToClient(hwnd, &mut point) }.as_bool();
        // A failed conversion is treated the same as "no button rect
        // published yet" — `click_through_hit_test`'s safe default
        // (`Client`) — rather than guessing at a point that might be
        // wrong, which could otherwise carve out the wrong region.
        let button_rect = converted.then(click_through_button_rect).flatten();
        // `click_through_on` is read explicitly (rather than the branch
        // condition's combined gate) because issue #232 added a second,
        // independent reason for this branch to run: a locked window with
        // click-through off must still fall through to
        // `click_through_hit_test`'s `false` — `PassThrough` — arm, not
        // skip the call outright.
        let click_through_on = CLICK_THROUGH_ENABLED.load(std::sync::atomic::Ordering::SeqCst);
        if let ClickThroughHitTest::Transparent =
            click_through_hit_test((point.x, point.y), button_rect, click_through_on)
        {
            return windows::Win32::Foundation::LRESULT(HTTRANSPARENT as isize);
        }
        // Click-through didn't claim this point (off entirely, inside the
        // button, or no rect published yet) — issue #232's resize lock
        // gets the next look, before falling through to `DefSubclassProc`
        // unchanged.
        if RESIZE_LOCKED.load(std::sync::atomic::Ordering::SeqCst) {
            debug_assert_eq!(HT_LEFT, HTLEFT as i32);
            debug_assert_eq!(HT_RIGHT, HTRIGHT as i32);
            debug_assert_eq!(HT_TOP, HTTOP as i32);
            debug_assert_eq!(HT_BOTTOM, HTBOTTOM as i32);
            debug_assert_eq!(HT_TOP_LEFT, HTTOPLEFT as i32);
            debug_assert_eq!(HT_TOP_RIGHT, HTTOPRIGHT as i32);
            debug_assert_eq!(HT_BOTTOM_LEFT, HTBOTTOMLEFT as i32);
            debug_assert_eq!(HT_BOTTOM_RIGHT, HTBOTTOMRIGHT as i32);
            debug_assert_eq!(HT_CLIENT, HTCLIENT as i32);

            // SAFETY: `hwnd`/`msg`/`wparam`/`lparam` are exactly what this
            // subclass procedure was called with. Forwarding them to
            // `DefSubclassProc` here — rather than only at the bottom of
            // this function — just computes the default hit test early so
            // `resize_lock_hit_test` can override it below; winit's own
            // handling underneath never sees this message a second time,
            // since this branch returns instead of falling through once it
            // takes this path.
            let default = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            let overridden = resize_lock_hit_test(default.0 as i32, true);
            return windows::Win32::Foundation::LRESULT(overridden as isize);
        }
        // Resize isn't locked either: falls through to `DefSubclassProc`
        // below unchanged, same as a normal hit test on a borderless
        // window.
    }

    // SAFETY: `hwnd`/`msg`/`wparam`/`lparam` are exactly what this
    // subclass procedure was called with; forwarding them unchanged to
    // `DefSubclassProc` is how every message this handler doesn't
    // explicitly veto keeps reaching winit's own window procedure.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Whether `window_proc`'s `WM_NCHITTEST` branch should carve out the
/// click-through button (issue #167 rehash). Mirrors `Settings::
/// click_through` — `ui.rs` calls `set_click_through` every time that
/// field changes: on the toggle-cluster button's own click, when
/// `OverlayApp::ui` re-applies a saved setting on its first frame, and
/// when a tray-issued turn-off request is synced. A plain atomic for the
/// same reason `TRAY_ICON_ADDED` is one: this crate's Win32 message pump
/// and `ui()` both run on the single UI thread, so there is no real
/// concurrency to protect against, just a way to read a value written
/// from a different call stack.
#[cfg(windows)]
static CLICK_THROUGH_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Issue #232: whether `window_proc`'s `WM_NCHITTEST` branch should
/// override every resize-border hit test to plain client area
/// (`resize_lock_hit_test`). Mirrors `Settings::always_on_top` — `ui.rs`
/// calls `set_resize_locked` every time that field changes: on the
/// toggle-cluster pin button's own click, and when `OverlayApp::ui`
/// re-applies a saved setting on its first frame. Same plain-atomic
/// reasoning as `CLICK_THROUGH_ENABLED`: this crate's Win32 message pump
/// and `ui()` both run on the single UI thread, so this is a way to read a
/// value written from a different call stack, not real concurrency.
#[cfg(windows)]
static RESIZE_LOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The click-through button's current hit box, in physical-pixel
/// client-area coordinates — the same space `ScreenToClient` converts a
/// `WM_NCHITTEST` point into. Published every frame by `toggle_cluster`
/// via `set_click_through_button_rect`, regardless of whether
/// click-through is currently on: the button can move between frames (a
/// resize, a DPI change) even while click-through is off, and publishing
/// unconditionally means `window_proc` never reads a rect stale enough to
/// belong to a previous layout. `CLICK_THROUGH_BUTTON_RECT_SET` is `false`
/// until the first such publish, which is what `click_through_hit_test`'s
/// `None` case exists for.
#[cfg(windows)]
static CLICK_THROUGH_BUTTON_LEFT: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static CLICK_THROUGH_BUTTON_TOP: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static CLICK_THROUGH_BUTTON_RIGHT: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static CLICK_THROUGH_BUTTON_BOTTOM: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static CLICK_THROUGH_BUTTON_RECT_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the tray menu's "Turn off click-through" command
/// (`request_click_through_off`) and taken (checked and cleared) by
/// `ui.rs`'s `OverlayApp::ui` once per frame via
/// `take_tray_click_through_off_request`, the same "poll a flag every
/// frame" shape the deleted `Ctrl+Alt+P` hotkey check used. The actual
/// OS-level passthrough is already off the instant the tray click lands —
/// `request_click_through_off` clears `CLICK_THROUGH_ENABLED` itself,
/// directly, for immediacy — so this flag exists only to catch `Settings::
/// click_through` and the toggle-cluster button's displayed state up to
/// what the window is already doing.
#[cfg(windows)]
static TRAY_CLICK_THROUGH_OFF_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Turns the `WM_NCHITTEST` click-through carve-out on or off (issue #167
/// rehash). See `CLICK_THROUGH_ENABLED`'s doc comment for the call sites.
#[cfg(windows)]
pub fn set_click_through(enabled: bool) {
    CLICK_THROUGH_ENABLED.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(windows))]
pub fn set_click_through(_enabled: bool) {}

/// Turns the `WM_NCHITTEST` resize-border override on or off (issue #232).
/// See `RESIZE_LOCKED`'s doc comment for the call sites.
#[cfg(windows)]
pub fn set_resize_locked(locked: bool) {
    RESIZE_LOCKED.store(locked, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(windows))]
pub fn set_resize_locked(_locked: bool) {}

/// Publishes the click-through button's current hit box, in physical
/// pixels, client-area-relative (issue #167 rehash) — see
/// `CLICK_THROUGH_BUTTON_LEFT`'s doc comment. `toggle_cluster` is the only
/// caller; it converts its egui `Rect` (points, already client-relative)
/// with `pixels_per_point` and pads it slightly before calling this, so
/// the hit target isn't razor-thin at its edges.
#[cfg(windows)]
pub fn set_click_through_button_rect(left: i32, top: i32, right: i32, bottom: i32) {
    CLICK_THROUGH_BUTTON_LEFT.store(left, std::sync::atomic::Ordering::SeqCst);
    CLICK_THROUGH_BUTTON_TOP.store(top, std::sync::atomic::Ordering::SeqCst);
    CLICK_THROUGH_BUTTON_RIGHT.store(right, std::sync::atomic::Ordering::SeqCst);
    CLICK_THROUGH_BUTTON_BOTTOM.store(bottom, std::sync::atomic::Ordering::SeqCst);
    CLICK_THROUGH_BUTTON_RECT_SET.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(windows))]
pub fn set_click_through_button_rect(_left: i32, _top: i32, _right: i32, _bottom: i32) {}

/// Reads back the click-through button's published hit box, or `None` if
/// `set_click_through_button_rect` has never run — see
/// `CLICK_THROUGH_BUTTON_LEFT`'s doc comment. `window_proc`'s
/// `WM_NCHITTEST` branch is the only reader.
#[cfg(windows)]
fn click_through_button_rect() -> Option<Rect> {
    if !CLICK_THROUGH_BUTTON_RECT_SET.load(std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    Some(Rect {
        left: CLICK_THROUGH_BUTTON_LEFT.load(std::sync::atomic::Ordering::SeqCst),
        top: CLICK_THROUGH_BUTTON_TOP.load(std::sync::atomic::Ordering::SeqCst),
        right: CLICK_THROUGH_BUTTON_RIGHT.load(std::sync::atomic::Ordering::SeqCst),
        bottom: CLICK_THROUGH_BUTTON_BOTTOM.load(std::sync::atomic::Ordering::SeqCst),
    })
}

/// Issue #183: whether the overlay's window should be OS-level
/// mouse-passthrough *right now* — the value `ui.rs` reconciles into
/// `egui::ViewportCommand::MousePassthrough` (winit's `WS_EX_TRANSPARENT`)
/// once per frame.
///
/// Why this exists on top of `window_proc`'s `WM_NCHITTEST` carve-out: an
/// `HTTRANSPARENT` reply is documented to forward the hit test only to
/// other windows *in the same thread*. It never hands the click to another
/// process's window, which is the entire point of click-through for a game
/// overlay — so the `WM_NCHITTEST` branch alone could not pass a click to
/// the game underneath no matter how correct its carve-out was, and the
/// toggle looked enabled while doing nothing. `WS_EX_TRANSPARENT` is the
/// bit that actually does it.
///
/// `WS_EX_TRANSPARENT` was rejected the first time round (see `ui.rs`'s
/// toggle-cluster section comment) because it is a whole-window flag with
/// no per-region carve-out, so the very button that turns click-through
/// back off went unclickable with it. That is what this function solves:
/// the flag is reconciled *every frame against the OS cursor's own
/// position*, so while the cursor is inside the published button hit box
/// the answer is `false` and the button is reachable exactly as before, and
/// it flips back to `true` the moment the cursor leaves. `OverlayApp::ui`
/// repaints at ~10Hz unconditionally, which is what keeps that
/// reconciliation running while the window itself receives no mouse input
/// at all.
///
/// The decision is `click_through_hit_test`'s, not a second copy of it: a
/// point that would hit-test `Transparent` is exactly a point the window
/// should not receive. So the safe defaults are shared too — click-through
/// off, no button rect published yet, or a failed coordinate conversion all
/// resolve to "not passthrough" rather than stranding the user under a
/// window they cannot click.
#[cfg(windows)]
pub fn click_through_passthrough_wanted() -> bool {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let click_through_on = CLICK_THROUGH_ENABLED.load(std::sync::atomic::Ordering::SeqCst);
    if !click_through_on {
        return false;
    }
    let raw = OVERLAY_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if raw == 0 {
        return false;
    }
    // `OVERLAY_HWND` stores the handle as a bare integer — the inverse of
    // `disable_aero_snap`'s store, same as `force_frame_recompute` does.
    let hwnd = HWND(raw as *mut std::ffi::c_void);

    let mut point = POINT::default();
    // SAFETY: `point` is a valid, correctly-sized out-param for the
    // duration of both calls; `hwnd` is the overlay's own window, alive for
    // the whole process, and `ScreenToClient` overwrites `point` in place
    // with the client-relative equivalent.
    let located = unsafe { GetCursorPos(&mut point) }.is_ok()
        && unsafe { ScreenToClient(hwnd, &mut point) }.as_bool();
    let button_rect = located.then(click_through_button_rect).flatten();
    matches!(
        click_through_hit_test((point.x, point.y), button_rect, true),
        ClickThroughHitTest::Transparent
    )
}

#[cfg(not(windows))]
pub fn click_through_passthrough_wanted() -> bool {
    false
}

/// Takes (checks and clears) a pending tray "Turn off click-through"
/// request — see `TRAY_CLICK_THROUGH_OFF_REQUESTED`'s doc comment.
/// `OverlayApp::ui` calls this once per frame.
#[cfg(windows)]
pub fn take_tray_click_through_off_request() -> bool {
    TRAY_CLICK_THROUGH_OFF_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(windows))]
pub fn take_tray_click_through_off_request() -> bool {
    false
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

/// Menu-item id for "Turn off click-through" (issue #167 rehash) — the
/// tray's belt-and-braces fallback for click-through, alongside the
/// always-clickable carve-out `window_proc`'s `WM_NCHITTEST` branch gives
/// the toggle-cluster button itself. A plain one-shot item rather than a
/// checkable toggle: the button already covers turning click-through *on*
/// (and back off, now that it's reachable in every state), so this only
/// ever needs to do the one thing a stranded user could still be asking
/// for — the same "least new machinery" reasoning `install_tray`'s
/// existing `MF_STRING`-only `AppendMenuW` calls already follow, with no
/// per-open state read (a checkable item's `MF_CHECKED` would need one)
/// to keep in sync.
#[cfg(any(windows, test))]
const TRAY_MENU_CLICK_THROUGH_OFF: u32 = 3;

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
    /// Turn click-through off (issue #167 rehash) — the tray's fallback
    /// for the case the toggle-cluster button's own carve-out ever fails
    /// or ends up off-screen. See `TRAY_MENU_CLICK_THROUGH_OFF`'s doc
    /// comment.
    ClickThroughOff,
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
        TRAY_MENU_CLICK_THROUGH_OFF => Some(TrayCommand::ClickThroughOff),
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
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

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

/// Resource id the application icon is embedded under (`ShinraMeter-BPSR.rc` /
/// `build.rs`, from `crates/app/assets/shinra.ico`). The fallback below stays
/// because a host build has no resource section at all.
#[cfg(windows)]
const APP_ICON_RESOURCE_ID: u16 = 2;

/// The `HICON` for the tray entry: the executable's own embedded icon if
/// there is one, otherwise Windows' stock application icon. Both are
/// resource-backed shared icons owned by the module/system, so neither needs
/// `DestroyIcon`.
#[cfg(windows)]
fn tray_icon_handle() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{HICON, IDI_APPLICATION, LoadIconW};

    // SAFETY: `GetModuleHandleW(None)` asks for this process's own executable
    // module, which is loaded for the process's whole lifetime; the handle it
    // returns is borrowed, not owned, so it must not be freed. A null return
    // is reported as `Err` rather than as a zero handle, so the failure case
    // is the `else` of this `if let` rather than a `.0 != 0` check.
    if let Ok(module) = unsafe { GetModuleHandleW(None) } {
        // `MAKEINTRESOURCE`: a numeric resource id is passed as a `PCWSTR`
        // whose pointer value *is* the id, not a pointer to a string.
        let name = windows::core::PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16);
        // SAFETY: `module` is this process's own module handle and `name` is
        // a well-formed `MAKEINTRESOURCE` id; a missing resource is reported
        // as an error, not undefined behaviour. `LoadIconW` takes an
        // `Option<HINSTANCE>`, a distinct newtype over the same pointer as
        // `HMODULE`, so the handle is converted rather than passed through.
        if let Ok(icon) = unsafe { LoadIconW(Some(module.into()), name) } {
            return icon;
        }
        log::info!(
            "no icon resource {APP_ICON_RESOURCE_ID} embedded in the executable; using the stock application icon in the notification area"
        );
    }

    // SAFETY: no module handle at all (`None`, which the binding turns into
    // the null `HINSTANCE` this used to pass explicitly) with an `IDI_*` id is
    // the documented way to load a stock system icon.
    match unsafe { LoadIconW(None, IDI_APPLICATION) } {
        Ok(icon) => icon,
        Err(err) => {
            log::warn!("LoadIconW(IDI_APPLICATION) failed; the tray entry will be iconless: {err}");
            HICON(std::ptr::null_mut())
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
        .zip("ShinraMeter-BPSR".encode_utf16().chain(std::iter::once(0)))
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
    // `app_driven_reposition` exemption is needed. `ShowWindow`'s
    // `#[must_use]` `BOOL` reports whether the window *was* visible, not
    // whether the call succeeded, so there is nothing to check.
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
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
    // neither can be vetoed by `window_proc`. Each `#[must_use]` `BOOL` is
    // dropped on purpose: `ShowWindow`'s reports the window's *previous*
    // visibility rather than success, and `SetForegroundWindow` is
    // best-effort — Windows refuses foreground activation from a process that
    // doesn't currently own it, and there is nothing useful to do about that
    // beyond leaving the window visible.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
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
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SW_RESTORE, SW_SHOW, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
        ShowWindow, USER_DEFAULT_SCREEN_DPI,
    };

    // Un-hide first: a reset that left the window invisible would defeat the
    // whole point of the command.
    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread. The `#[must_use]` `BOOL`s are dropped for the
    // same reason as in `restore_from_tray`: they report prior visibility,
    // not success.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
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
            if let Err(err) = unsafe { GetWindowRect(hwnd, &mut current) } {
                log::warn!("GetWindowRect failed ({err}); can't reset the overlay's window");
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
    // SAFETY: same as above; `SWP_NOZORDER` means the `hWndInsertAfter`
    // argument is unused — hence `None`.
    let moved = app_driven_reposition(|| unsafe {
        SetWindowPos(
            hwnd,
            None,
            target.left,
            target.top,
            target.width(),
            target.height(),
            SWP_NOZORDER,
        )
    });
    if let Err(err) = moved {
        log::warn!("SetWindowPos failed while resetting the overlay's window: {err}");
        return;
    }
    // SAFETY: same as above; best-effort, see `restore_from_tray`.
    let _ = unsafe { SetForegroundWindow(hwnd) };
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
        .is_ok()
            && AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_CLICK_THROUGH_OFF as usize,
                windows::core::w!("Turn off click-through"),
            )
            .is_ok()
            && AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_EXIT as usize,
                windows::core::w!("Exit"),
            )
            .is_ok()
    };
    if !appended {
        log::warn!("AppendMenuW failed; the tray menu would be incomplete, so it isn't shown");
        // SAFETY: `menu` is a live, unowned menu handle not attached to any
        // window, so destroying it here is the only cleanup needed. The
        // result is dropped: this is already the failure path, and a leaked
        // menu handle is not something the app can do anything about.
        let _ = unsafe { DestroyMenu(menu) };
        return;
    }

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid, correctly-sized out-param.
    if let Err(err) = unsafe { GetCursorPos(&mut cursor) } {
        log::warn!("GetCursorPos failed ({err}); showing the tray menu at the screen origin");
    }

    // SAFETY: `hwnd` is the window this subclass is installed on, alive and
    // on the current thread; `TPM_RETURNCMD` makes `TrackPopupMenu` return
    // the selected id (or 0) instead of posting a `WM_COMMAND`.
    // `nReserved` is an `Option<i32>` documented as "must be zero", so `None`
    // — which the binding turns into that zero — is the honest spelling. The
    // three `#[must_use]` results around the `TrackPopupMenu` call are dropped
    // deliberately: `SetForegroundWindow` is best-effort (see
    // `restore_from_tray`), `PostMessageW` is the KB135788 dismissal nudge,
    // and `DestroyMenu` tears down a menu that is already finished with —
    // none of the three has a recovery worth writing.
    let selected = unsafe {
        let _ = SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        selected
    };

    match tray_command_for(selected.0 as u32) {
        Some(TrayCommand::ResetWindow) => reset_window(hwnd),
        Some(TrayCommand::ClickThroughOff) => request_click_through_off(),
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
            if let Err(err) = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
                log::warn!(
                    "PostMessageW(WM_CLOSE) failed ({err}); the tray Exit command did nothing"
                );
            }
        }
        None => {}
    }
}

/// The tray menu's "Turn off click-through" command (issue #167 rehash) —
/// see `TRAY_MENU_CLICK_THROUGH_OFF`'s doc comment for why this exists
/// alongside the toggle-cluster button's own always-clickable carve-out.
///
/// Clears `CLICK_THROUGH_ENABLED` directly, here, rather than only raising
/// `TRAY_CLICK_THROUGH_OFF_REQUESTED` and waiting for `ui.rs` to act on it
/// next frame: this is the fallback for when the carve-out itself might be
/// unreliable, so the actual OS-level passthrough should stop the instant
/// the tray command runs, independent of whether or when another frame
/// happens to render. The flag is raised too, so `OverlayApp::ui` still
/// syncs `Settings::click_through` and the button's displayed state to
/// match on its next frame — see `TRAY_CLICK_THROUGH_OFF_REQUESTED`'s doc
/// comment.
#[cfg(windows)]
fn request_click_through_off() {
    CLICK_THROUGH_ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    TRAY_CLICK_THROUGH_OFF_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
    log::info!("tray: turned click-through off");
}

/// The pointer's current position in screen space (egui points), sourced
/// directly from the OS rather than reconstructed from the window `ui.rs` is
/// itself moving.
///
/// Issue #68: `ui.rs`'s manual move/resize gestures (issue #11 — see this
/// module's top-level doc comment for why they can't use
/// `ViewportCommand::StartDrag`/`BeginResize`) used to derive the
/// screen-space pointer as `outer_rect.min + pointer.latest_pos()`: the
/// window's own origin plus egui's window-local pointer position. That
/// reconstruction is a feedback loop for any gesture that moves the
/// window's origin (`Move`, and the resize directions that drag a
/// corner/edge through it) — Windows doesn't synthesize a mouse-move
/// message when a window moves under a stationary cursor, so the
/// window-local position egui reports stays frozen between real mouse
/// events while `outer_rect.min` keeps advancing as the gesture itself
/// drives the window. The reconstructed screen position therefore drifts by
/// exactly the distance the window just moved on the previous frame, so
/// `pointer - start_pointer` grows every single frame even with the mouse
/// held perfectly still, and the gesture accelerates itself into runaway
/// growth.
///
/// `GetCursorPos` sidesteps this entirely: it reads the OS's own cursor
/// position, which nothing the window does can perturb. This is the same
/// call winit itself makes in its own `handle_os_dragging` before entering a
/// native `SC_MOVE`/`SC_SIZE` loop. `ui.rs`'s `window_and_pointer` calls this
/// first and only falls back to the old window-relative reconstruction when
/// it returns `None` — non-Windows dev hosts, where the gesture is cosmetic
/// and the drift this fixes is harmless.
///
/// Returns `None` (after logging at `warn`) if `GetCursorPos` fails, which
/// in practice is effectively impossible — it has no documented failure mode
/// short of a broken process/desktop state — so the log call needs no
/// rate-limiting.
#[cfg(windows)]
pub fn cursor_position(pixels_per_point: f32) -> Option<egui::Pos2> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    // SAFETY: `point` is a valid, correctly-sized out-param for the
    // duration of this call.
    if let Err(err) = unsafe { GetCursorPos(&mut point) } {
        log::warn!(
            "GetCursorPos failed ({err}); falling back to the window-relative pointer reconstruction"
        );
        return None;
    }
    Some(cursor_points(point.x, point.y, pixels_per_point))
}

#[cfg(not(windows))]
pub fn cursor_position(_pixels_per_point: f32) -> Option<egui::Pos2> {
    None
}

/// Converts a physical-pixel cursor position, as `GetCursorPos` reports it,
/// to egui points. Guards against a non-finite or non-positive
/// `pixels_per_point` by treating it as `1.0` (identity conversion) instead
/// of dividing by zero or producing a NaN/infinite coordinate that would
/// send a gesture's delta flying.
///
/// Pure so the conversion is unit-testable off-Windows, the same way this
/// module's other geometry helpers are.
#[cfg(any(windows, test))]
fn cursor_points(x: i32, y: i32, pixels_per_point: f32) -> egui::Pos2 {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    egui::Pos2::new(x as f32 / scale, y as f32 / scale)
}

/// Writes `image` to the system clipboard as an image (issue #82's Share
/// button — `ui.rs`'s `handle_screenshot_events` calls this once an
/// `Event::Screenshot` reply arrives). Windows-gated like the rest of this
/// module, per the app's own convention.
///
/// Issue #183: this used to swallow both failure modes into a `log::warn!`
/// and return `()`, which is why "the Copy button does nothing" was
/// indistinguishable from "the Copy button worked" without a log file. It
/// now returns the failure to the caller, which raises it as a
/// `StatusLine::Error` banner — the same one-line surface the packet
/// pipeline's errors already use — so a clipboard that could not be opened
/// (another process holding it is routine on Windows) says so on screen.
///
/// `arboard::Clipboard::new` opens (and implicitly closes, on `Drop`) a
/// fresh clipboard handle per call rather than caching one on `OverlayApp`:
/// this fires at most once per click, so the overhead is irrelevant, and a
/// short-lived handle can never be blamed for holding the system clipboard
/// open across frames.
#[cfg(windows)]
pub fn write_clipboard_image(image: &egui::ColorImage) -> Result<(), String> {
    let [width, height] = image.size;
    let data = arboard::ImageData {
        width,
        height,
        bytes: std::borrow::Cow::Borrowed(image.as_raw()),
    };
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|err| format!("could not open the system clipboard: {err}"))?;
    clipboard
        .set_image(data)
        .map_err(|err| format!("could not write the screenshot to the clipboard: {err}"))
}

#[cfg(not(windows))]
pub fn write_clipboard_image(_image: &egui::ColorImage) -> Result<(), String> {
    Err("clipboard image writes are only implemented on Windows".to_string())
}

/// UTF-16, NUL-terminated — the shape every Win32 string parameter in this
/// module wants (`WinHTTP`'s `PCWSTR`s in `http_get`, the `OPENFILENAMEW`
/// fields in `choose_log_export_path`). One helper rather than one per
/// caller: the two used to carry byte-identical private copies.
///
/// The returned buffer owns the bytes a `PCWSTR`/`PWSTR` built from it
/// points at, so it must stay alive for as long as the call using it.
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens the native "Save As" common dialog so the user picks where the
/// header dropdown's "Export logs" item (`ui::draw_header_menu`, issue
/// #220) writes the bundled log file — issue #220 is explicit that this
/// crate must never pick a fixed or hidden destination itself (logs may
/// carry player names and other identifying traffic, per `logging`'s
/// module doc comment, so silently dropping a file somewhere is worse than
/// just not exporting). `default_filename` seeds the dialog's file name box
/// (`logging::EXPORT_DEFAULT_FILENAME`); the user can still rename it
/// before saving.
///
/// `GetSaveFileNameW` (the classic common dialog), not `IFileSaveDialog`
/// (the modern COM one): this crate has no COM initialization anywhere
/// today, and the COM dialog needs one (`CoInitializeEx`) plus a new
/// `Win32_System_Com` feature — for a plain "pick a file to save" prompt
/// with none of `IFileSaveDialog`'s extras (custom places, multiple
/// simultaneous filters) actually needed here.
///
/// Synchronous and blocking, deliberately unlike `http_get`'s "always call
/// from a spawned thread" rule: `GetSaveFileNameW` is itself a modal
/// dialog — the OS already blocks input to its owner window for as long as
/// it's up — so calling it straight from `ui.rs`'s click handler adds no
/// blocking that wasn't already going to happen.
///
/// That reasoning covers this dialog and nothing after it (PR #227
/// review): the export's own copy can move up to two files of
/// `logging::MAX_LOG_BYTES` each, which the OS is *not* already blocking
/// the frame thread on, so `ui::start_log_export` runs it on a spawned
/// thread once this function has returned a destination.
///
/// Owned by the cached `OVERLAY_HWND` when it's available (same
/// load-and-guard shape as `force_frame_recompute`) so the dialog is modal
/// to the overlay window; falls back to no owner if it hasn't been cached
/// yet rather than failing outright.
///
/// Returns `None` if the user cancels, closes the dialog, or the call
/// otherwise fails (e.g. `CommDlgExtendedError`-reported failure) — the
/// caller treats every one of those the same: do nothing.
#[cfg(windows)]
pub fn choose_log_export_path(default_filename: &str) -> Option<std::path::PathBuf> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows::core::{PCWSTR, PWSTR};

    let raw = OVERLAY_HWND.load(std::sync::atomic::Ordering::SeqCst);
    // Inverse of the store in `disable_aero_snap` — see
    // `force_frame_recompute`'s identical cast/guard for why a zero raw
    // value means "not cached yet" rather than a literal null HWND to pass
    // through.
    let owner = if raw == 0 {
        HWND(std::ptr::null_mut())
    } else {
        HWND(raw as *mut std::ffi::c_void)
    };

    // `lpstrFile` is in/out: seeded with the suggested name below, and
    // overwritten with the user's chosen full path on success. 32767 is
    // the documented practical ceiling for this API's buffer (the NTFS
    // long-path limit), so it comfortably fits any real save location.
    let mut file_buf = vec![0u16; 32767];
    let default_wide = wide(default_filename);
    file_buf[..default_wide.len()].copy_from_slice(&default_wide);

    let filter = wide("Log files\0*.log\0All files\0*.*\0");
    let title = wide("Export ShinraMeter-BPSR logs");
    let def_ext = wide("log");

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: file_buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrDefExt: PCWSTR(def_ext.as_ptr()),
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
        ..Default::default()
    };

    // SAFETY: every pointer field set above (`lpstrFilter`, `lpstrFile`,
    // `lpstrTitle`, `lpstrDefExt`) is backed by a `Vec<u16>` that outlives
    // this call and, for `lpstrFile`, is sized well past anything
    // `GetSaveFileNameW` could write back into it. `hwndOwner` is either a
    // still-live handle cached from winit's own window (see
    // `force_frame_recompute`'s identical reasoning) or the null default,
    // both valid `HWND` values for this call.
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if !ok.as_bool() {
        return None;
    }

    let end = file_buf.iter().position(|&unit| unit == 0).unwrap_or(0);
    if end == 0 {
        return None;
    }
    Some(std::path::PathBuf::from(String::from_utf16_lossy(
        &file_buf[..end],
    )))
}

/// Non-Windows stub — see `choose_log_export_path`'s doc comment. Dev/CI
/// hosts for this crate are Linux, so there is no native save dialog to
/// call; `ui.rs`'s click handler treats `None` as "the user didn't pick a
/// destination" either way, which is also the correct behavior here.
#[cfg(not(windows))]
pub fn choose_log_export_path(_default_filename: &str) -> Option<std::path::PathBuf> {
    None
}

/// Issue #171: the manual "Check for updates" header-menu item needs an
/// HTTPS GET to GitHub's releases API, and — same reasoning as every other
/// function in this module — egui/eframe expose no HTTP client at all, so
/// this reaches straight for the OS's own one rather than pulling in a new
/// HTTP/TLS crate. WinHTTP (not WinINet, which is documented as unsuitable
/// for services/background callers) is the same client Windows' own
/// Update/Store stack uses, needs no extra system DLL beyond `winhttp.dll`
/// (present on every supported Windows version), and, via `WinHttpOpen`'s
/// `pszAgentW`, is the one place a `User-Agent` header can be set before
/// the connection even exists — GitHub's API rejects any request that
/// arrives without one.
///
/// `host` and `path` are supplied separately (rather than one URL string)
/// because that is exactly the shape `WinHttpConnect`/`WinHttpOpenRequest`
/// want; always HTTPS (`WINHTTP_FLAG_SECURE`), on port 443
/// (`INTERNET_DEFAULT_HTTPS_PORT`) — this function has no plain-HTTP path,
/// on purpose, since its one caller only ever talks to `api.github.com`.
///
/// Synchronous and blocking by design: `update_check::check_for_update`
/// (the sole caller) is itself only ever run from a spawned
/// `std::thread`, never the UI thread, so there is nothing here for a
/// blocking call to stall — see that function's doc comment.
#[cfg(windows)]
pub fn http_get(host: &str, path: &str, user_agent: &str) -> Result<String, String> {
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::Networking::WinHttp::{
        INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
        WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    };
    use windows::core::PCWSTR;

    /// Closes a WinHTTP handle (session, connection or request — the same
    /// `WinHttpCloseHandle` closes all three) when dropped, so every early
    /// `return Err(...)` below still releases whatever handles were opened
    /// before it, instead of a leak per error path.
    struct WinHttpHandle(*mut core::ffi::c_void);

    impl Drop for WinHttpHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was returned by a prior WinHTTP `*Open*`
                // call and is only ever closed once, here, at end of scope.
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    // SAFETY (this whole function): every WinHTTP call below is handed
    // either a `PCWSTR` built from a `wide()` buffer that outlives the
    // call, or a handle owned by a `WinHttpHandle` still in scope — never a
    // dangling or foreign pointer. None of these calls touch the window or
    // its `HWND`, so nothing here interacts with `OVERLAY_HWND` or the
    // reposition-exemption machinery elsewhere in this module.
    let agent = wide(user_agent);
    let session = unsafe {
        WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    };
    if session.is_null() {
        return Err(format!(
            "WinHttpOpen failed (GetLastError={})",
            unsafe { GetLastError() }.0
        ));
    }
    let session = WinHttpHandle(session);

    // Every WinHTTP timeout has to be set explicitly: the defaults leave
    // name resolution on 0, which WinHTTP documents as "infinite", so a
    // stalled resolver, a captive portal or a firewall that drops rather
    // than refuses would block this thread — and, with it, the header
    // dropdown's "Checking…" state — forever. This is a user-triggered
    // check against a single small JSON endpoint, so a few seconds each is
    // plenty: 5s to resolve, 5s to connect, 10s to send and 10s to receive.
    // Set on the session handle, before `WinHttpConnect`, so every handle
    // derived from it inherits them.
    unsafe { WinHttpSetTimeouts(session.0, 5_000, 5_000, 10_000, 10_000) }
        .map_err(|err| format!("WinHttpSetTimeouts failed: {err}"))?;

    let host_w = wide(host);
    let connect = unsafe {
        WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        )
    };
    if connect.is_null() {
        return Err(format!(
            "WinHttpConnect failed (GetLastError={})",
            unsafe { GetLastError() }.0
        ));
    }
    let connect = WinHttpHandle(connect);

    let verb = wide("GET");
    let object = wide(path);
    let request = unsafe {
        WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(object.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    if request.is_null() {
        return Err(format!(
            "WinHttpOpenRequest failed (GetLastError={})",
            unsafe { GetLastError() }.0
        ));
    }
    let request = WinHttpHandle(request);

    unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }
        .map_err(|err| format!("WinHttpSendRequest failed: {err}"))?;
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
        .map_err(|err| format!("WinHttpReceiveResponse failed: {err}"))?;

    let mut status: u32 = 0;
    let mut status_len = std::mem::size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut core::ffi::c_void),
            &mut status_len,
            std::ptr::null_mut(),
        )
    }
    .map_err(|err| format!("WinHttpQueryHeaders failed: {err}"))?;
    if status != 200 {
        return Err(format!("GitHub releases API returned HTTP {status}"));
    }

    // Drained in a loop, per WinHTTP's own documented pattern:
    // `WinHttpQueryDataAvailable` reports how much of the *next* chunk is
    // ready (never the whole body at once), and a `0` is WinHTTP's own
    // end-of-response signal.
    let mut body = Vec::new();
    loop {
        let mut available: u32 = 0;
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(|err| format!("WinHttpQueryDataAvailable failed: {err}"))?;
        if available == 0 {
            break;
        }
        let mut chunk = vec![0u8; available as usize];
        let mut read: u32 = 0;
        unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut core::ffi::c_void,
                available,
                &mut read,
            )
        }
        .map_err(|err| format!("WinHttpReadData failed: {err}"))?;
        chunk.truncate(read as usize);
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|err| format!("response was not valid UTF-8: {err}"))
}

/// Non-Windows stub — see `http_get`'s doc comment. Dev/CI hosts for this
/// crate are Linux, so `update_check`'s tests exercise only the pure
/// parsing/comparison logic and never call this at all; a real "is an
/// update available" answer needs a real Windows build.
#[cfg(not(windows))]
pub fn http_get(_host: &str, _path: &str, _user_agent: &str) -> Result<String, String> {
    Err("update checks are only supported on Windows builds".to_string())
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

    /// The crash this exists to stop: an external `MoveWindow` (the uidbg
    /// harness, a window manager, anything) asks for a window taller than
    /// Win32 will grant, Win32 clamps the height to `SHRT_MAX`, and the
    /// 32767px swapchain that follows blows past egui-wgpu's 8192
    /// `max_texture_dimension_2d` and panics inside `Surface::configure`.
    #[test]
    fn an_i16_max_height_proposal_is_clamped_to_what_the_renderer_can_present() {
        assert_eq!(
            clamped_window_extent(460, i32::from(i16::MAX)),
            Some((460, MAX_WINDOW_EXTENT_PX))
        );
    }

    #[test]
    fn an_oversize_width_is_clamped_on_its_own_axis() {
        assert_eq!(
            clamped_window_extent(40_000, 420),
            Some((MAX_WINDOW_EXTENT_PX, 420))
        );
    }

    #[test]
    fn both_axes_clamp_together() {
        assert_eq!(
            clamped_window_extent(i32::MAX, i32::MAX),
            Some((MAX_WINDOW_EXTENT_PX, MAX_WINDOW_EXTENT_PX))
        );
    }

    /// Anything the renderer can actually present has to pass through
    /// untouched, so the clamp never perturbs an ordinary move/resize —
    /// including a window sized exactly at the limit.
    #[test]
    fn a_presentable_size_is_left_alone() {
        for (cx, cy) in [
            (460, 420),
            (1, 1),
            (MAX_WINDOW_EXTENT_PX, MAX_WINDOW_EXTENT_PX),
        ] {
            assert_eq!(clamped_window_extent(cx, cy), None, "{cx}x{cy}");
        }
    }

    /// Win32 uses non-positive extents in some proposals (and an
    /// `SWP_NOSIZE` proposal leaves the fields meaningless entirely); this
    /// is a ceiling only, so it must not invent a floor of its own —
    /// minimum sizing is `MIN_INNER_SIZE`'s job, over in `ui.rs`.
    #[test]
    fn a_non_positive_extent_is_not_the_clamps_business() {
        assert_eq!(clamped_window_extent(0, -1), None);
    }

    // -- decode_swp_flags / oversize_proposal_log (issue #119) ------------

    #[test]
    fn decode_swp_flags_names_a_single_bit() {
        assert_eq!(decode_swp_flags(0x0002), "NOMOVE");
    }

    #[test]
    fn decode_swp_flags_names_every_set_bit_in_declaration_order() {
        // NOSIZE | NOMOVE | NOZORDER | NOACTIVATE, the shape a full
        // move+resize with z-order and activation pinned would carry.
        assert_eq!(
            decode_swp_flags(0x0001 | 0x0002 | 0x0004 | 0x0010),
            "NOSIZE|NOMOVE|NOZORDER|NOACTIVATE"
        );
    }

    #[test]
    fn decode_swp_flags_reports_no_bits_set_explicitly() {
        // A full move+resize with nothing pinned — the shape an external
        // MoveWindow/SetWindowPos sends, and the case #119's diagnostics
        // most need to be unambiguous about.
        assert_eq!(decode_swp_flags(0), "NONE");
    }

    #[test]
    fn decode_swp_flags_appends_unrecognized_bits_as_hex() {
        assert_eq!(decode_swp_flags(0x0002 | 0x0800), "NOMOVE|0x0800");
    }

    /// The exact issue-#119 shape: a 460x32767 proposal, no gesture in
    /// flight, current rect and DPI both available.
    #[test]
    fn oversize_proposal_log_reports_the_issue_119_shape() {
        let message = oversize_proposal_log(
            ProposedWindowPos {
                x: 10,
                y: 20,
                cx: 460,
                cy: i32::from(i16::MAX),
            },
            (460, MAX_WINDOW_EXTENT_PX),
            0,
            Some(rect(10, 20, 470, 420)),
            Some(96),
            false,
        );
        assert!(
            message.contains("proposed=(x=10, y=20, 460x32767)"),
            "{message}"
        );
        assert!(
            message.contains(&format!("clamped_to=460x{MAX_WINDOW_EXTENT_PX}")),
            "{message}"
        );
        assert!(message.contains("flags=NONE"), "{message}");
        assert!(
            message.contains("current_window=460x400 at (10, 20)"),
            "{message}"
        );
        assert!(message.contains("dpi=96"), "{message}");
        assert!(
            message.contains("app_driven_reposition_active=false"),
            "{message}"
        );
    }

    #[test]
    fn oversize_proposal_log_reports_a_gesture_in_flight() {
        let message = oversize_proposal_log(
            ProposedWindowPos {
                x: 0,
                y: 0,
                cx: 9000,
                cy: 9000,
            },
            (MAX_WINDOW_EXTENT_PX, MAX_WINDOW_EXTENT_PX),
            0x0002,
            Some(rect(0, 0, 9000, 9000)),
            Some(96),
            true,
        );
        assert!(
            message.contains("app_driven_reposition_active=true"),
            "{message}"
        );
        assert!(message.contains("flags=NOMOVE"), "{message}");
    }

    #[test]
    fn oversize_proposal_log_reports_a_missing_current_rect_and_dpi() {
        let message = oversize_proposal_log(
            ProposedWindowPos {
                x: 0,
                y: 0,
                cx: 9000,
                cy: 9000,
            },
            (MAX_WINDOW_EXTENT_PX, MAX_WINDOW_EXTENT_PX),
            0,
            None,
            None,
            false,
        );
        assert!(
            message.contains("current_window=<unavailable>"),
            "{message}"
        );
        assert!(message.contains("dpi=<unavailable>"), "{message}");
    }

    // -- format_elapsed (issue #119) ---------------------------------------

    #[test]
    fn format_elapsed_reports_the_two_coarsest_units() {
        assert_eq!(format_elapsed(std::time::Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(std::time::Duration::from_secs(65)), "1m 5s");
        assert_eq!(
            format_elapsed(std::time::Duration::from_secs(3_665)),
            "1h 1m"
        );
        assert_eq!(
            format_elapsed(std::time::Duration::from_secs(90_000)),
            "1d 1h"
        );
    }

    // -- LAST_OVERSIZE_PROPOSAL / last_oversize_proposal (issue #119) -----
    //
    // One test, not two: both cases share the same process-global
    // `LAST_OVERSIZE_PROPOSAL`, and cargo runs `#[test]` fns concurrently
    // by default, so splitting this into a round-trip test and a separate
    // poison-recovery test would race on that global instead of exercising
    // it deterministically.
    #[test]
    fn last_oversize_proposal_round_trips_and_survives_a_poisoned_lock() {
        store_last_oversize_proposal("test proposal one".to_string());
        let read = last_oversize_proposal().expect("a proposal was just stored");
        assert!(
            read.starts_with("last oversize window proposal ("),
            "{read}"
        );
        assert!(read.contains("test proposal one"), "{read}");

        // Poison the lock the way a real panic would: panic while holding
        // the guard, on a call `catch_unwind` contains so the poisoning
        // doesn't fail this test itself.
        let lock = LAST_OVERSIZE_PROPOSAL.get_or_init(|| std::sync::Mutex::new(None));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.lock().unwrap();
            panic!("deliberately poisoning the lock for a test");
        }));
        assert!(panicked.is_err());
        assert!(lock.is_poisoned());

        // The whole point of `PoisonError::into_inner` over `unwrap`/`?`
        // here: the reader must recover the poisoned lock and still return
        // the value stored before the poisoning, not panic or return
        // `None`.
        let read_after_poison = last_oversize_proposal()
            .expect("poison recovery must still return the previously stored value");
        assert!(
            read_after_poison.contains("test proposal one"),
            "{read_after_poison}"
        );

        // And a poisoned lock must not wedge future stores either.
        store_last_oversize_proposal("test proposal two".to_string());
        let read_final =
            last_oversize_proposal().expect("store after recovering from a poison must work");
        assert!(read_final.contains("test proposal two"), "{read_final}");
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
        assert_eq!(
            tray_command_for(TRAY_MENU_CLICK_THROUGH_OFF),
            Some(TrayCommand::ClickThroughOff)
        );
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
    fn click_through_off_passes_every_point_through_to_default() {
        // Click-through off is the common case, and must never be
        // second-guessed by this subclass at all — `PassThrough` means
        // `window_proc` doesn't touch the hit test, whatever `button_rect`
        // says.
        let button = rect(0, 0, 20, 20);
        assert_eq!(
            click_through_hit_test((5, 5), Some(button), false),
            ClickThroughHitTest::PassThrough
        );
        assert_eq!(
            click_through_hit_test((500, 500), Some(button), false),
            ClickThroughHitTest::PassThrough
        );
        assert_eq!(
            click_through_hit_test((5, 5), None, false),
            ClickThroughHitTest::PassThrough
        );
    }

    #[test]
    fn click_through_on_keeps_the_button_itself_clickable() {
        let button = rect(10, 10, 30, 30);
        assert_eq!(
            click_through_hit_test((10, 10), Some(button), true),
            ClickThroughHitTest::Client,
            "top-left corner is inclusive"
        );
        assert_eq!(
            click_through_hit_test((20, 20), Some(button), true),
            ClickThroughHitTest::Client
        );
        assert_eq!(
            click_through_hit_test((29, 29), Some(button), true),
            ClickThroughHitTest::Client,
            "just inside the exclusive bottom-right edge"
        );
    }

    #[test]
    fn click_through_on_makes_everywhere_else_transparent() {
        let button = rect(10, 10, 30, 30);
        assert_eq!(
            click_through_hit_test((0, 0), Some(button), true),
            ClickThroughHitTest::Transparent
        );
        assert_eq!(
            click_through_hit_test((30, 30), Some(button), true),
            ClickThroughHitTest::Transparent,
            "the bottom-right edge is exclusive, so it's already outside"
        );
        assert_eq!(
            click_through_hit_test((1000, 1000), Some(button), true),
            ClickThroughHitTest::Transparent
        );
    }

    #[test]
    fn click_through_on_with_no_published_rect_defaults_to_client() {
        // Safe-by-construction: a `None` rect means `toggle_cluster` hasn't
        // run its first frame yet. Resolving that to `Transparent` would
        // make the *whole* window click-through with no carve-out at all
        // for however long that startup gap lasts — exactly the stranding
        // this mechanism exists to prevent — so `Client` (i.e. behave as
        // if click-through were off) is the only safe default.
        assert_eq!(
            click_through_hit_test((0, 0), None, true),
            ClickThroughHitTest::Client
        );
        assert_eq!(
            click_through_hit_test((99999, 99999), None, true),
            ClickThroughHitTest::Client
        );
    }

    /// Issue #232: while the overlay is pinned, Windows' own edge-drag
    /// resize (driven by `WS_THICKFRAME`, entirely independent of `ui.rs`'s
    /// manual resize gesture) must not be able to resize the window either
    /// — every one of the eight resize-border hit-test codes is overridden
    /// to plain client area.
    #[test]
    fn resize_lock_hit_test_overrides_every_resize_border_when_locked() {
        for code in RESIZE_BORDER_HIT_TESTS {
            assert_eq!(resize_lock_hit_test(code, true), HT_CLIENT);
        }
    }

    #[test]
    fn resize_lock_hit_test_leaves_resize_borders_alone_when_unlocked() {
        for code in RESIZE_BORDER_HIT_TESTS {
            assert_eq!(resize_lock_hit_test(code, false), code);
        }
    }

    /// Anything that isn't one of the eight resize-border codes — plain
    /// client area, and (stood in for by `2`) `HTCAPTION`, which this
    /// borderless window never actually produces but which must stay inert
    /// too if it ever did — passes through unchanged even while locked,
    /// since none of those grow or shrink the window.
    #[test]
    fn resize_lock_hit_test_never_touches_non_resize_codes() {
        for code in [HT_CLIENT, 0, 2] {
            assert_eq!(resize_lock_hit_test(code, true), code);
        }
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

    #[test]
    fn cursor_points_at_unit_scale_is_identity() {
        assert_eq!(cursor_points(100, 200, 1.0), egui::pos2(100.0, 200.0));
    }

    #[test]
    fn cursor_points_divides_by_the_scale_factor() {
        assert_eq!(cursor_points(150, 300, 1.5), egui::pos2(100.0, 200.0));
    }

    #[test]
    fn cursor_points_treats_a_zero_or_nan_scale_as_1_0() {
        assert_eq!(cursor_points(100, 200, 0.0), egui::pos2(100.0, 200.0));
        assert_eq!(cursor_points(100, 200, f32::NAN), egui::pos2(100.0, 200.0));
    }
}
