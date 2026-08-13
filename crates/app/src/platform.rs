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

#[cfg(windows)]
pub fn disable_aero_snap(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_MAXIMIZEBOX,
    };

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
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_MAXIMIZEBOX.0 as isize));
    }
    log::info!("cleared WS_MAXIMIZEBOX to disable Aero Snap on drag");
}

#[cfg(not(windows))]
pub fn disable_aero_snap(_cc: &eframe::CreationContext<'_>) {}
