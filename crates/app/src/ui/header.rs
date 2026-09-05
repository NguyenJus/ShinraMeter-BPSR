//! Header surface: title/subtitle band, toggles, emblem, wash, separators,
//! and the screenshot/share plumbing the header's share button drives.

use super::*;

/// Returns whether the Share button (in the toggle cluster painted at the
/// end of this function) fired a screenshot request this frame — issue #96
/// (PR #98 review): the caller uses this to know whether to stash this same
/// frame's row-bottom bound into `OverlayApp::pending_screenshot_bound`.
// Issue #156's new `capturing` parameter pushes this to 8 genuinely
// independent dependencies (egui plumbing, the snapshot, the command
// channel, the settings handle, icons, the drag gesture, and now the
// screenshot guard) — `settings` already bundles two of what would
// otherwise be separate parameters for the same reason. One more scalar
// flag doesn't earn a second bundling struct of its own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    gesture: &mut WindowGesture,
    // Issue #340: last frame's measured header band (`OverlayApp::header_
    // rect`), or `None` before the first frame — the same value the
    // resize-border double-click and the "Reset to defaults"/default-size
    // helpers size against via `measured_header_band_height`, so the band
    // this frame paints (and the wash it derives from) can never disagree
    // with what those other paths think the header occupies.
    previous_header_rect: Option<egui::Rect>,
    // Issue #156: whether a Share screenshot capture is currently in
    // flight (the request was fired and its `Event::Screenshot` reply has
    // not landed yet) — threaded down to `toggle_cluster` so the toggle
    // buttons can suppress their hover fill and tooltip on the frame that
    // actually gets captured. See `screenshot_capture_guard`'s doc comment
    // for why this can't just be "the frame the click happened on".
    capturing: bool,
    // Issue #219: whether the Share button may fire a screenshot capture
    // this frame — true on `OverlayView::Live` and on `OverlayView::History`
    // once a specific historical encounter is open, false on the bare
    // history list. Threaded down to `toggle_cluster`, which gates both the
    // button's `Sense` (via `toggle_button`'s `enabled`) and its glyph tint
    // (`toggle_state_tint`) on it — the same disabled treatment the History
    // button already wears when history is unavailable. Computed by
    // `share_active_for_view` in `OverlayApp::ui`, the only place that can
    // see `self.view`.
    share_active: bool,
    // Issue #171: the manual "Check for updates" header-menu item's
    // in-flight/last-result state — threaded through to `draw_header_menu`,
    // the only place that reads or mutates it.
    update_check: &mut UpdateCheckState,
    // Issue #220: where the "Export logs" or "Export session bundle" item's
    // spawned copy thread reports back to — threaded through to
    // `draw_header_menu`, the only place that clones it.
    tx_log_export: &Sender<LogExportOutcome>,
    // Issue #350 (S2): forwarded straight through to `draw_header_menu`,
    // the only place that writes to it — see that parameter's doc comment.
    log_exports_in_flight: &mut usize,
    // Issue #39: whether the history thread exists at all — threaded
    // through to `toggle_cluster` (issue #186 moved the History control
    // there from `draw_header_menu`) so its History button can be disabled
    // (rather than absent) when history is off in settings.json or its
    // database could not be opened.
    has_history: bool,
    // Issue #39: set by the toggle cluster's History button (issue #186);
    // read by `OverlayApp::ui` to switch `self.view` into
    // `OverlayView::History`.
    open_history: &mut bool,
    // Issue #39: when `Some`, the two header text lines name a *historical*
    // fight rather than the live encounter (spec DECISION D7). Everything
    // else about the header — timer, DPS readout, toggle cluster, dropdown —
    // is identical in both modes.
    history: Option<HistoryHeader<'_>>,
    // Issue #321: forwarded straight through to `draw_header_menu`, the
    // only place that writes to it — see that parameter's doc comment.
    quit_requested: &mut bool,
) -> bool {
    let (title, subtitle) = header_text(snapshot, history.as_ref());
    // The header band's height budget — also what `draw_header_wash` and the
    // paint clips below size themselves against, so the whole band is one
    // number derived once rather than several that could drift apart.
    let band_height = measured_header_band_height(previous_header_rect);

    // The panel's own full-width rect, captured before the band below
    // narrows it to the drag band's height — the background wash (issue #59,
    // #62, #81, #91) is anchored off the panel's left/top/right edges and
    // sized to the whole drag band, so it ends exactly where the first
    // player row begins.
    let panel = ui.available_rect_before_wrap();
    // The header band's full paint extent — `panel` truncated to
    // `band_height`, with no top adjustment. `RESIZE_EDGE` is a
    // hit-test-only concern (see `band` below), not a paint boundary; the
    // panel's own fill/border already cover that strip.
    let header_paint_clip =
        egui::Rect::from_min_size(panel.min, egui::vec2(panel.width(), band_height));
    // The height the gutter emblem's box is *centered* on — the header's
    // text rows, which is the block the mark decorates. Not what it is
    // clipped to: that is `header_paint_clip`, the whole band (see the blit
    // below).
    let text_band_height = header_text_band_height();
    // The whole header band is the drag surface — title line, subtitle
    // line, and the timer/DPS/buttons row — registered *before* the
    // row's contents so the buttons drawn into it end up on top and still get
    // their clicks. Grabbing a single glyph was too small a target to hit.
    let band = {
        let mut rect = header_paint_clip;
        // Leave the top resize strip alone — a drag surface spanning it would
        // win the hit test and swallow every north-edge resize.
        rect.min.y += RESIZE_EDGE;
        rect
    };

    // The decorative background wash, painted before anything else in the
    // band so every later layer — emblem, title, separator, chevron,
    // subtitle, stat row — sits on top of it. Sized to the whole drag band
    // (issue #91), not the text rows alone (issue #81) and not the fixed
    // 98pt run before that: the gradient and the oversized emblem it carries
    // are the header's background, so they run behind the stat-pill row too.
    //
    // Issue #158 (offset corrected by #297): `band_height` alone stops
    // `2 * ITEM_SPACING_Y + SEPARATOR_HEIGHT` (10pt) short of the first
    // player row — `OverlayApp::ui` puts a `ui.separator()` between the
    // header and the rows, and egui's vertical layout pays its ordinary
    // `ITEM_SPACING_Y` gap on both sides of it (once landing the separator,
    // once landing the first row after it), none of which is inside the
    // band. Sizing the wash to just `band_height` left that strip showing
    // the bare panel fill (with the separator's faint line inside it)
    // between the wash's bottom edge and the first row — a hard, visible
    // cutoff, not a fade, and (#297) a seam once either region carries its
    // own background image rather than the same-colored default artwork.
    // Extending to `first_player_row_top_offset` — the same function
    // `default_inner_height` sums for the window's default open height —
    // closes that gap and keeps the two derivations from ever drifting
    // apart again. Never a literal.
    let wash_height = first_player_row_top_offset(band_height) - HEADER_WASH_INSET;
    // Issue #252: `settings` carries the overlay-opacity slider the wash's
    // own fills and emblem fade with, so it stays at its baked-in alpha no
    // longer — the same threading the header background image (issue #121)
    // already needed the whole `Settings` for. The gutter emblem below is
    // painted from this same slider, matching the `PANEL_FILL`/
    // `PANEL_BORDER_COLOR` gamma-multiply above and the skill window's own
    // opacity threading (issue #184).
    let opacity = Opacity::new(settings.settings.opacity);
    draw_header_wash(ui, panel, icons, wash_height, settings.settings);

    // Issue #183: pinning the overlay locks its *position* as well as its
    // Z-order, so the drag band refuses to start a move while
    // `Settings::always_on_top` is set, and says so with its cursor.
    let drag_locked = drag_locked_by_pin(settings.settings.always_on_top);
    let drag_surface = ui.interact(band, ui.id().with("title_bar"), egui::Sense::drag());
    if drag_surface.hovered() {
        ctx.set_cursor_icon(if drag_locked {
            egui::CursorIcon::NotAllowed
        } else {
            egui::CursorIcon::Grab
        });
    }
    // Once per gesture: this only captures the anchor the drag is measured
    // against. The actual per-frame repositioning is `drive_window_gesture`.
    if !drag_locked && drag_surface.drag_started_by(egui::PointerButton::Primary) {
        begin_window_gesture(ctx, gesture, GestureKind::Move);
    }

    // Title is always rendered (even as the "No target" placeholder) so the
    // header's height never jitters between frames. So is the subtitle row
    // (issue #91): it renders empty when the scene is unknown rather than
    // being skipped, because skipping it collapsed the band by
    // `SUBTITLE_LINE_HEIGHT + ITEM_SPACING_Y` and lifted the whole stat-pill
    // row every time the app fell back to its idle "No target" state.
    let title_row = draw_title_line(ui, &title);
    // The gutter emblem (issue #59): bled off the left edge and centered on
    // the header's *text* rows (`text_band_height`), but clipped to the
    // *whole band* (`header_paint_clip`, issue #91). The two are deliberately
    // different heights and neither is a typo for the other.
    //
    // Issue #75 clipped this to the text band as well, to keep the mark's
    // lower blade off the timer's readout underneath. That cost the diamond
    // its bottom corner: the box runs 14pt past the text band, so a text-band
    // clip visibly chopped the mark's point off mid-blade. The stat row is
    // what moved instead (`HEADER_STAT_ROW_INSET_X`) — it now starts right
    // of the emblem's right edge, so the two share the same rows without
    // touching, and the clip is free to be the band's. It is a floor, not a
    // trim: all it still stops is ink running on into the first player row.
    //
    // The mark's *top* corner is still cut, by the panel's own edge — see
    // `header_emblem_rect`, where that asymmetry is the design.
    if let Some(emblem) = icons.glyphs.get(GlyphIcon::Emblem) {
        ui.painter().with_clip_rect(header_paint_clip).image(
            emblem.id(),
            header_emblem_rect(title_row, text_band_height),
            UV_FULL,
            opacity.apply(HEADER_EMBLEM_COLOR),
        );
    }
    for (segment_rect, color) in title_separator_segments(title_separator_rect(title_row)) {
        ui.painter().rect_filled(segment_rect, 0.0, color);
    }
    // The menu control (issue #54, #71), in the strip at the right of the
    // title row that `header_text_rect` keeps clear. Registered after the
    // title-bar drag surface above, so a click on it opens the dropdown
    // instead of starting a window move. Always points down — it is a menu
    // affordance now, not a collapse-state indicator (see `menu_chevron`).
    // The title row's own toggle pill (issue #185): click-through and
    // always-on-top, immediately left of the chevron. Registered after the
    // drag surface (so its clicks never start a window move) and before the
    // chevron, which it does not overlap.
    title_row_toggles(
        ui,
        SettingsHandle {
            settings: &mut *settings.settings,
            tx_settings: settings.tx_settings,
        },
        icons,
        title_row,
        capturing,
    );
    // Issue #183: a pin click above can land *mid-drag* — the pointer is
    // necessarily still down on the header band that started the move — so
    // the in-flight gesture is cancelled here, on the same frame, before
    // `OverlayApp::ui`'s `drive_window_gesture` gets to advance it. Gating
    // the gesture's *start* alone would have let that one drag run to
    // completion after the window was supposedly locked.
    cancel_move_gesture_when_pinned(gesture, settings.settings.always_on_top);
    let chevron_response = menu_chevron(ui, chevron_rect(title_row));
    // Issue #120: the dropdown's drill-down page lives in egui temp memory
    // (`menu_page_id`), so it has to be cleared while the popup is shut or
    // a menu closed on the Columns page would reopen there — the first
    // click after opening would land somewhere the user never asked for.
    // Cleared here, outside the popup body, because the body only runs
    // while the popup is open; `Popup::default_response_id` is exactly the
    // id `Popup::menu(&chevron_response)` below keys its open state under.
    if !egui::Popup::is_id_open(ctx, egui::Popup::default_response_id(&chevron_response)) {
        reset_menu_page(ctx);
    }
    // `CloseOnClickOutside` rather than the default `CloseOnClick` (issue
    // #93, now a standing rule — see `menu.rs`'s issue #120 block): this
    // menu holds toggles, a slider and page-navigation rows, so no click
    // inside it may dismiss it implicitly. The action rows call
    // `ui.close()` themselves to still dismiss on click.
    // `settings` (a `SettingsHandle<'_>`) is reborrowed rather than moved
    // into this closure — issue #167 added a second use of it below
    // (`toggle_cluster`), and `SettingsHandle`'s `&mut Settings` field
    // can't be moved out twice. `&mut *settings.settings` is an ordinary
    // reborrow through that field, scoped to just this `.show(...)` call,
    // so `settings` is still whole and usable afterward.
    egui::Popup::menu(&chevron_response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            draw_header_menu(
                ui,
                ctx,
                tx_command,
                SettingsHandle {
                    settings: &mut *settings.settings,
                    tx_settings: settings.tx_settings,
                },
                previous_header_rect,
                icons,
                update_check,
                tx_log_export,
                log_exports_in_flight,
                quit_requested,
            );
        });
    draw_subtitle_line(ui, subtitle.as_deref().unwrap_or(""));

    // The gap above the stat row is `HEADER_STAT_ROW_GAP`, not the layout's
    // own `ITEM_SPACING_Y` — see that constant for why, and
    // `header_band_height` for the budget it feeds. egui already inserted
    // one `ITEM_SPACING_Y` after the row above, so only the remainder is
    // added here.
    ui.add_space(HEADER_STAT_ROW_GAP - ITEM_SPACING_Y);
    ui.horizontal(|ui| {
        // The whole row is inset from the panel's left content edge
        // (`HEADER_STAT_ROW_INSET_X`, issue #91). `add_space` in a
        // horizontal layout advances the cursor directly, with no
        // `item_spacing` of its own, so the timer's ink starts exactly this
        // far in.
        ui.add_space(HEADER_STAT_ROW_INSET_X);
        // Three stat readouts (issue #56), replacing the bare
        // "clock icon + 02:39 | X DPS | Y DMG" run this row used to be. The
        // gap between them is the layout's own `item_spacing.x`
        // (`apply_theme` sets 6.0), which is exactly the "small gap" the
        // reference render shows — no manual spacer.
        //
        // The reference's trailing " DPS"/" DMG" words are gone: the icon
        // plus the `/s` suffix already say which number is which, and the
        // words cost more width than the whole pill chrome does.
        stat_pill(
            ui,
            StatPill::timer(
                &fmt_duration(snapshot.duration_ms),
                icons.glyphs.get(GlyphIcon::Timer).map(|t| t.id()),
            ),
        );
        stat_pill(
            ui,
            StatPill::header(
                &format!("{}/s", fmt_short(snapshot.total_dps as i64)),
                icons.glyphs.get(GlyphIcon::Speed).map(|t| t.id()),
            ),
        );
        // Total damage for the fight (reference render's e.g. "30.10B"). The
        // heart icon is the reference's own choice of glyph here; despite it
        // this is `snapshot.total_damage` and nothing else — there is no
        // party-HP figure anywhere in this codebase.
        stat_pill(
            ui,
            StatPill::header(
                &fmt_short(snapshot.total_damage),
                icons.glyphs.get(GlyphIcon::Heart).map(|t| t.id()),
            ),
        );
        toggle_cluster(
            ui,
            tx_command,
            icons,
            capturing,
            share_active,
            has_history,
            open_history,
        )
    })
    .inner
}

// -- toggle cluster (issue #62, #82, #167) --------------------------------
//
// The source's fourth stat-row cell: a click-through LED, a cloud-upload LED
// and a queue gauge, in a 22pt pill. We had none of those features, so all
// three used to render **in their off state and inert** — no click
// handling, no settings, no tooltip. Issue #62 was explicit that a use for
// these slots would be decided later; issue #82 repurposed the click-through
// and cloud-upload LEDs as real buttons (Share — copy a screenshot to the
// clipboard — and Reset, moved out of the header dropdown), leaving only the
// queue gauge ring inert, since there was still no upload queue for it to
// show. Issue #167 dropped that still-inert ring entirely (there is still no
// queue, and none planned) and added two real, unrelated toggles in its
// place and one new slot alongside it: OS-level mouse click-through and
// runtime always-on-top (`ViewportCommand::WindowLevel`).
//
// Issue #185 then split that four-button pill in two: click-through and
// always-on-top moved up to the title row, into their own oval left of the
// dropdown chevron (`title_row_toggles`), and issue #186 filled one of the
// slots they freed with History, moved out of the header dropdown. So this
// pill now holds three one-shot actions — Share, Reset, History — the title
// row's holds the two on/off toggles, and nothing anywhere in either is
// decorative.
//
// Click-through (issue #167 rehash) is *not* `ViewportCommand::
// MousePassthrough` — that sets `WS_EX_TRANSPARENT` on the whole window
// with no per-region carve-out, so once it was on, the very button that
// turns it back off became unclickable too, with only a keyboard hotkey
// (which needs focus that click-through's whole point is to give away) as
// an escape hatch. Instead, `platform::set_click_through` flips a flag
// `platform::window_proc`'s `WM_NCHITTEST` handling reads directly: it
// reports `HTTRANSPARENT` for the whole window *except* the click-through
// button's own hit box (published every frame below via `platform::
// set_click_through_button_rect`), which always reports normal
// (`HTCLIENT`) hit-testing. The button is therefore reachable with the
// mouse in every state — it is deliberately excluded from its own
// passthrough — and the tray menu's "Turn off click-through" entry
// (`platform::install_tray`) exists only as a belt-and-braces fallback for
// the case that carve-out ever fails or the window ends up off-screen.

/// Tint a toggle-cluster button paints with while its state is "off" —
/// white at a fraction of `TOGGLE_ACTIVE_COLOR`'s alpha (issue #251, raised
/// again by issue #255's live-window pass). The original value (`0x11`, the
/// still-inert queue ring's borrowed stroke color, source `OffBrush=
/// "#1fff"`) read as ~7% opacity and made the click-through/always-on-top
/// buttons nearly invisible when off. `0x40` — exactly half of
/// `TOGGLE_ACTIVE_COLOR`'s `0x80` — fixed that but, against the header
/// chrome's `#25282f`, still landed under the 3:1 WCAG minimum for a
/// non-text UI component (the "on" tint measures 4.7-5.4:1). `0x50`, this
/// constant's next value, was assumed to clear 3:1 and does not: composited
/// over the real backgrounds this pill paints on — bare `PANEL_FILL`
/// rgb(18, 18, 22) and the header wash blended over it, at full opacity and
/// at the shipped 200/255 default alike — it measures 2.65-2.84:1. `0x58` is
/// the first alpha over 3:1 in every one of those cases; `0x60` measures
/// 3.30-3.53:1, clearing the minimum with margin to spare while staying
/// visibly dimmer than the "on" tint's `0x80`, so the two states stay
/// distinguishable at a glance.
pub(crate) const TOGGLE_OFF_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x60);
/// Tint the toggle cluster's buttons are painted with while active — the
/// same half-white `TOOLBAR_ICON_TINT` every other clickable icon in this
/// module uses. Share and Reset (one-shot actions, not on/off state) always
/// paint at this tint; click-through and always-on-top use it only in their
/// "on" state (`toggle_state_tint`).
pub(crate) const TOGGLE_ACTIVE_COLOR: egui::Color32 = TOOLBAR_ICON_TINT;
/// Filled circle painted behind the click-through icon while click-through
/// is enabled (issue #292): `toggle_state_tint`'s on/off distinction is
/// only a ~25% alpha delta on the same white glyph (`TOGGLE_OFF_COLOR` vs
/// `TOGGLE_ACTIVE_COLOR`), which reads as nearly identical at a glance —
/// not obvious enough for a control that flips a system-wide mouse
/// passthrough mode. An amber wash (the color already associated with
/// "caution, behavior changed") is a second, unmistakable signal on top of
/// the existing tint, painted only in the "on" state; "off" is unchanged
/// (relies on the pill's own `PILL_FILL` plus the dim icon tint, same as
/// before this issue).
pub(crate) const CLICK_THROUGH_ON_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(233, 196, 106, 130);
/// Circular hover wash painted behind a toggle-cluster button, matching the
/// oval pill's own shape rather than a foreign square badge.
pub(crate) const TOGGLE_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 30);
pub(crate) const TOGGLE_MOUSE_SIDE: f32 = 12.0;
pub(crate) const TOGGLE_CLOUD_SIDE: f32 = 14.0;
/// Click-through button glyph side (issue #167) — same 14pt as `TOGGLE_
/// CLOUD_SIDE`/the old queue slot, keeping every non-Share button in the
/// cluster the same visual size.
pub(crate) const TOGGLE_CLICK_THROUGH_SIDE: f32 = 14.0;
/// Always-on-top button glyph side (issue #167) — see `TOGGLE_CLICK_
/// THROUGH_SIDE`'s doc comment.
pub(crate) const TOGGLE_ALWAYS_ON_TOP_SIDE: f32 = 14.0;
/// History button glyph side (issue #186) — the same 14pt as every other
/// non-Share button in the cluster, see `TOGGLE_CLICK_THROUGH_SIDE`.
pub(crate) const TOGGLE_HISTORY_SIDE: f32 = 14.0;
pub(crate) const TOGGLE_GAP: f32 = 5.0;
pub(crate) const TOGGLE_PAD_X: f32 = 4.0;

/// Gap, in points, between the title row's toggle pill (issue #185) and the
/// dropdown chevron's reserved strip to its right. `TOGGLE_PAD_X`'s value,
/// deliberately: the pill's own internal padding is what sets the rhythm the
/// chevron then continues, so the two read as one run of controls rather
/// than two clusters that happen to be adjacent.
pub(crate) const TITLE_TOGGLE_GAP_X: f32 = TOGGLE_PAD_X;

/// Width of the title row's toggle pill (issue #185): the click-through and
/// always-on-top buttons, laid out with exactly the padding, glyph sides and
/// inter-button gap they had inside `toggle_cluster`, so the two ovals still
/// read as one family after the move.
pub(crate) const TITLE_TOGGLE_PILL_WIDTH: f32 =
    2.0 * TOGGLE_PAD_X + TOGGLE_CLICK_THROUGH_SIDE + TOGGLE_GAP + TOGGLE_ALWAYS_ON_TOP_SIDE;

/// Width the *title* row keeps clear at its right end (issue #185): the
/// chevron's own `HEADER_RIGHT_CONTROL_WIDTH` strip, plus the toggle pill
/// that now sits immediately left of it and the gap between them. Its own
/// constant rather than a wider `HEADER_RIGHT_CONTROL_WIDTH` because the
/// pill lives on the title row alone — the subtitle row still reserves only
/// the chevron strip, and widening the shared constant would have punched a
/// 45pt hole in the area name for no reason.
pub(crate) const TITLE_RIGHT_CONTROLS_WIDTH: f32 =
    HEADER_RIGHT_CONTROL_WIDTH + TITLE_TOGGLE_GAP_X + TITLE_TOGGLE_PILL_WIDTH;
/// Points the click-through button's published hit box (`platform::
/// set_click_through_button_rect`) is padded out by on every side, so the
/// `WM_NCHITTEST` carve-out that keeps it reachable under click-through
/// (issue #167 rehash) isn't razor-thin right at the glyph's edges.
pub(crate) const CLICK_THROUGH_HIT_PAD: f32 = 2.0;

/// Horizontal offset, in points, from the title row's toggle pill's left
/// edge to the left edge of the click-through button's glyph box. Issue #185
/// moved this button out of the stat row's toggle cluster and into the title
/// pill, where click-through is the *first* slot, so the offset is now the
/// pill's own left padding and nothing else — still spelled as its own
/// constant so the hit box can be computed before the pill is painted (see
/// `click_through_button_slot`).
pub(crate) const CLICK_THROUGH_SLOT_OFFSET_X: f32 = TOGGLE_PAD_X;

/// The click-through button's glyph box, in points, derived from the title
/// row's toggle pill rect (`title_toggle_pill_rect`). Pure so
/// `title_row_toggles` can publish the button's `WM_NCHITTEST` hit box
/// (issue #167 rehash) before it starts painting — and so the geometry is
/// unit-testable on every platform, unlike the `cfg(windows)` publish
/// itself.
pub(crate) fn click_through_button_slot(pill: egui::Rect) -> egui::Rect {
    egui::Rect::from_center_size(
        egui::pos2(
            pill.left() + CLICK_THROUGH_SLOT_OFFSET_X + TOGGLE_CLICK_THROUGH_SIDE / 2.0,
            pill.center().y,
        ),
        egui::Vec2::splat(TOGGLE_CLICK_THROUGH_SIDE),
    )
}

/// Converts the click-through button's egui rect (points, already
/// client-area-relative — matching what `ScreenToClient` produces) into the
/// physical-pixel `left, top, right, bottom` bounds `platform::
/// set_click_through_button_rect` publishes for `WM_NCHITTEST`. Padded by
/// `CLICK_THROUGH_HIT_PAD` points first so the carve-out isn't razor-thin at
/// the glyph's edges.
///
/// A non-finite or non-positive `pixels_per_point` is treated as `1.0`, the
/// same guard (and for the same reason) as `platform::cursor_points`:
/// multiplying by `0.0`/NaN would round every bound to `0`, and a
/// zero-area rect can never `contains` anything — so `click_through_hit_test`
/// would resolve `Transparent` for *every* point, including the button that
/// turns click-through back off. Identity scaling leaves the button a few
/// pixels off; a degenerate rect strands the user.
///
/// Pure so the points-to-pixels transform is unit-testable off-Windows; the
/// `cfg`-gated publish call is the only Windows-only part.
pub(crate) fn click_through_hit_box_px(
    button_rect: egui::Rect,
    pixels_per_point: f32,
) -> (i32, i32, i32, i32) {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let padded = button_rect.expand(CLICK_THROUGH_HIT_PAD);
    (
        (padded.left() * scale).round() as i32,
        (padded.top() * scale).round() as i32,
        (padded.right() * scale).round() as i32,
        (padded.bottom() * scale).round() as i32,
    )
}

/// One toggle-cluster button's hit box, hover highlight and accessible
/// label. The painted glyph itself is the caller's job — Share and Reset
/// draw from two different icon sets (`GlyphIcon` and `ToolbarIcon`) — so
/// this only owns the interaction chrome shared by both. Same
/// hand-supplied `widget_info`/`on_hover_text` shape as `menu_chevron`, and
/// for the same reason: a raw `interact` `Response` carries no `WidgetInfo`
/// from anywhere.
///
/// `capturing` (issue #156) suppresses both the hover circle and the
/// tooltip while a Share screenshot capture is in flight: the pointer is
/// necessarily still over whichever button was clicked to start the
/// capture, so on the frame that actually gets captured (see
/// `screenshot_capture_guard`'s doc comment — it's *not* the click frame
/// itself) both would otherwise paint straight into the image. Applies to
/// every button in the cluster, not just Share, since `capturing` is one
/// flag shared by the whole row.
///
/// `enabled` (PR #197 review) is the widget's real interactive state, not a
/// paint hint: `false` drops it to `Sense::hover()`, so it registers no
/// click and accesskit publishes no `Action::Click`, and reports
/// `enabled: false` in the `WidgetInfo`, which is what makes a screen
/// reader announce it as disabled. That is the `add_enabled(..)` behaviour
/// the History item had back in `draw_header_menu`; gating only the click's
/// *effect* at the call site left the button sounding perfectly usable
/// while doing nothing.
pub(crate) fn toggle_button(
    ui: &egui::Ui,
    rect: egui::Rect,
    label: &str,
    capturing: bool,
    enabled: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("toggle_cluster", label)),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if response.hovered() && !capturing {
        ui.painter()
            .circle_filled(rect.center(), rect.width() / 2.0 + 2.0, TOGGLE_HOVER_FILL);
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled && response.enabled(),
            label,
        )
    });
    if capturing {
        response
    } else {
        response.on_hover_text(label)
    }
}

/// Which tint a toggle-cluster on/off button paints its glyph with (issue
/// #167): `TOGGLE_ACTIVE_COLOR` — the same half-white every clickable icon
/// in this cluster uses — while the state is on, `TOGGLE_OFF_COLOR` — a
/// dimmer but still clearly legible white (issue #251) — while off.
/// Pure so the state -> color mapping is unit-testable without a live
/// `egui::Context`; `toggle_cluster` is the only caller.
pub(crate) fn toggle_state_tint(active: bool) -> egui::Color32 {
    if active {
        TOGGLE_ACTIVE_COLOR
    } else {
        TOGGLE_OFF_COLOR
    }
}

/// The click-through tray escape hatch (issue #167 rehash). The button
/// itself is always clickable now (`platform::window_proc`'s `WM_NCHITTEST`
/// carve-out — see the section comment above), so this is only the
/// belt-and-braces fallback for when the tray's "Turn off click-through"
/// entry fires: `requested` is whatever `OverlayApp::ui` read from
/// `platform::take_tray_click_through_off_request` this frame.
///
/// Unlike the deleted `Ctrl+Alt+P` hotkey this replaced, a tray request
/// only ever turns click-through *off* — there's no tray item for turning
/// it on, since the always-reachable button already owns that — so this is
/// not a toggle: a request forces `false` outright, and an idle frame
/// leaves `click_through` exactly as it found it. Pure so this is
/// unit-testable without a live `egui::Context` or the platform layer's
/// real atomics.
pub(crate) fn click_through_after_tray_request(click_through: bool, requested: bool) -> bool {
    if requested { false } else { click_through }
}

/// Paints the stat row's toggle cluster: Share, Reset (issue #82) and
/// History (issue #186) — three one-shot actions in one `PILL_FILL` oval,
/// matching the DPS/damage pills' chrome. Click-through and always-on-top
/// used to live here too; issue #185 moved them to the title row's own pill
/// (`title_row_toggles`), and History took one of the slots they freed.
///
/// Returns whether Share was clicked this frame (issue #96, PR #98 review)
/// — `draw_header` propagates this up so `OverlayApp::ui` knows to stash
/// this frame's row-bottom bound for the async screenshot reply.
///
/// `capturing` (issue #156) is threaded straight through to every
/// `toggle_button` call so the whole cluster suppresses its hover fill and
/// tooltip together while a capture is in flight, rather than special-casing
/// Share alone.
///
/// `has_history`/`open_history` (issue #186) are the History item's old
/// `draw_header_menu` parameters, rerouted here with the button: the cluster
/// no longer needs a `SettingsHandle` at all, since the only two controls
/// that flipped a setting are the ones that left.
///
/// `share_active` (issue #219) is whether Share may fire at all this frame
/// — see `share_active_for_view`'s doc comment. Gated the same way
/// `has_history` gates History: `toggle_button`'s `enabled` flag (so a
/// disabled Share senses no click and accesskit reports it disabled) plus
/// `toggle_state_tint` for the dimmed glyph, rather than hiding the button
/// outright — hiding it would shift every button to its right on the
/// frames it's inactive.
/// The "<label>" / "<label>: unavailable" shape shared by every toggle
/// whose label doubles as its own disabled-state explanation (Share's
/// `share_active`, History's `has_history`, PR #225 review of issue #219)
/// — pulled out so the wording can't drift apart between the two call
/// sites by a hand-edit to just one of them.
pub(crate) fn availability_label(
    active: bool,
    label: &'static str,
    unavailable: &'static str,
) -> &'static str {
    if active { label } else { unavailable }
}

pub(crate) fn toggle_cluster(
    ui: &mut egui::Ui,
    tx_command: &Sender<UiCommand>,
    icons: &Icons,
    capturing: bool,
    share_active: bool,
    has_history: bool,
    open_history: &mut bool,
) -> bool {
    let height = ui.spacing().interact_size.y;
    let width = 2.0 * TOGGLE_PAD_X
        + TOGGLE_MOUSE_SIDE
        + TOGGLE_GAP
        + TOGGLE_CLOUD_SIDE
        + TOGGLE_GAP
        + TOGGLE_HISTORY_SIDE;
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    if !ui.is_rect_visible(rect) {
        return false;
    }
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, PILL_FILL);

    let mut x = rect.left() + TOGGLE_PAD_X;
    let y = rect.center().y;

    // Share (issue #82): captures the app window as rendered this instant
    // and copies it to the system clipboard as an image, replacing a manual
    // screenshot. The capture is asynchronous — this only fires the
    // request; `handle_screenshot_events` picks up the resulting
    // `Event::Screenshot` on a later frame and does the actual clipboard
    // write.
    let share_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_MOUSE_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_MOUSE_SIDE),
    );
    let mut screenshot_requested = false;
    let share_label = availability_label(
        share_active,
        "Copy screenshot to clipboard",
        "Copy screenshot to clipboard: unavailable",
    );
    if toggle_button(ui, share_rect, share_label, capturing, share_active).clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        screenshot_requested = true;
    }
    if let Some(share) = icons.glyphs.get(GlyphIcon::Share) {
        ui.painter().image(
            share.id(),
            share_rect,
            UV_FULL,
            toggle_state_tint(share_active),
        );
    }
    x += TOGGLE_MOUSE_SIDE + TOGGLE_GAP;

    // Reset (issue #82): moved out of the header dropdown — `draw_header_menu`
    // used to be its only trigger — into a one-click slot here, reusing the
    // same `ToolbarIcon::Reset` texture and the same `UiCommand::Reset`.
    let reset_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_CLOUD_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_CLOUD_SIDE),
    );
    if toggle_button(ui, reset_rect, "Reset", capturing, true).clicked() {
        let _ = tx_command.try_send(UiCommand::Reset);
    }
    if let Some(reset) = icons.toolbar.get(ToolbarIcon::Reset) {
        ui.painter()
            .image(reset.id(), reset_rect, UV_FULL, TOGGLE_ACTIVE_COLOR);
    }
    x += TOGGLE_CLOUD_SIDE + TOGGLE_GAP;

    // History (issue #186): moved out of the header dropdown into the slot
    // issue #185 freed by taking click-through to the title row. Same
    // `open_history` flag the menu item used to set, read back by
    // `OverlayApp::ui` to switch `self.view` into `OverlayView::History`.
    //
    // The disabled state the menu item carried (`add_enabled(has_history,
    // ..)`, for history off in settings.json or a database that could not be
    // opened) survives the move as `toggle_state_tint(has_history)` — the
    // same dim "off" tint the click-through and always-on-top buttons wear,
    // which is what "inert" already looks like in this chrome — plus
    // `toggle_button`'s `enabled` flag, which carries the half of
    // `add_enabled` the tint cannot (PR #197 review): gating only the
    // click's effect here left the widget sensing clicks and telling
    // accesskit it was enabled, so a screen reader announced a usable
    // button that did nothing. The tooltip is what explains it, so the
    // button is still self-describing rather than silently inert.
    let history_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_HISTORY_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_HISTORY_SIDE),
    );
    let history_label = availability_label(has_history, "History", "History: unavailable");
    if toggle_button(ui, history_rect, history_label, capturing, has_history).clicked() {
        *open_history = true;
    }
    if let Some(history) = icons.glyphs.get(GlyphIcon::History) {
        ui.painter().image(
            history.id(),
            history_rect,
            UV_FULL,
            toggle_state_tint(has_history),
        );
    }

    screenshot_requested
}

/// Paints the title row's toggle pill (issue #185): click-through and
/// always-on-top, moved out of `toggle_cluster` into their own stadium
/// immediately left of the dropdown chevron, wearing the same `PILL_FILL`
/// chrome and the same `toggle_button`/`toggle_state_tint` treatment they
/// had in the stat row. Only the position changed.
///
/// Registered with `ui.interact` on explicit rects rather than allocated,
/// exactly like `menu_chevron`: this lives *inside* the title row
/// `draw_title_line` already allocated, in the strip `title_text_rect` keeps
/// clear for it. It is registered after `draw_header`'s title-bar drag
/// surface, so it wins the hit test over it and clicking a toggle never
/// starts a window drag.
///
/// It is also the one publisher of the click-through button's
/// `WM_NCHITTEST` hit box, which followed the button here — see the
/// invariant below.
pub(crate) fn title_row_toggles(
    ui: &mut egui::Ui,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    title_row: egui::Rect,
    capturing: bool,
) {
    let SettingsHandle {
        settings,
        tx_settings,
    } = settings;

    let rect = title_toggle_pill_rect(title_row, ui.spacing().interact_size.y);

    // Invariant: the published hit box always describes *this* frame's
    // layout, even on a frame egui culls the pill — publishing before the
    // visibility early return below is what guarantees it, because a culled
    // frame that returned early would leave `WM_NCHITTEST` consulting a rect
    // from a previous layout and a click at the button's real position would
    // resolve `Transparent`, stranding the user under click-through. Clearing
    // the rect on the culled path was the alternative; publishing wins
    // because it keeps the button reachable rather than merely un-stale.
    // See `click_through_hit_box_px` for the points-to-pixels conversion and
    // `platform::CLICK_THROUGH_BUTTON_LEFT` for why it's unconditional.
    let click_through_rect = click_through_button_slot(rect);
    let (hit_left, hit_top, hit_right, hit_bottom) =
        click_through_hit_box_px(click_through_rect, ui.ctx().pixels_per_point());
    crate::platform::set_click_through_button_rect(hit_left, hit_top, hit_right, hit_bottom);

    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, PILL_FILL);

    // Click-through (issue #167; mechanism replaced — issue #167 rehash,
    // then completed by issue #183): OS-level mouse passthrough for the
    // whole overlay *except this very button*, which stays clickable in
    // every state by design — see the toggle-cluster section comment above
    // for the carve-out, and `platform::click_through_passthrough_wanted`
    // for why `WM_NCHITTEST` alone could never pass a click to another
    // process and what now does. `GlyphIcon::MouseOff`'s literal "mouse
    // disabled" glyph is the closest fit the vendored set has. Persisted to
    // `Settings::click_through` so it survives a restart, re-applied on
    // `OverlayApp`'s first frame. The tray menu's "Turn off click-through"
    // entry is a belt-and-braces fallback, not the primary way back — this
    // button is.
    let click_through_label = if settings.click_through {
        "Click-through: on"
    } else {
        "Click-through: off"
    };
    // Issue #292: a second, color-based on/off signal painted behind the
    // icon (see `CLICK_THROUGH_ON_FILL`'s doc comment) — before
    // `toggle_button` so its own hover wash still paints on top of it.
    if settings.click_through {
        ui.painter().circle_filled(
            click_through_rect.center(),
            click_through_rect.width() / 2.0 + 3.0,
            CLICK_THROUGH_ON_FILL,
        );
    }
    if toggle_button(ui, click_through_rect, click_through_label, capturing, true).clicked() {
        settings.toggle_click_through();
        crate::platform::set_click_through(settings.click_through);
        let _ = tx_settings.send(settings.clone());
    }
    if let Some(mouse_off) = icons.glyphs.get(GlyphIcon::MouseOff) {
        ui.painter().image(
            mouse_off.id(),
            click_through_rect,
            UV_FULL,
            toggle_state_tint(settings.click_through),
        );
    }

    // Always-on-top (issue #167): whether the overlay stays pinned above
    // other windows, via `ViewportCommand::WindowLevel` — the runtime
    // counterpart to `viewport()`'s hardcoded `.with_always_on_top()`,
    // which only ever sets the *initial* level a fresh process opens with.
    // Issue #183: it now also locks the window's *position* — see
    // `drag_locked_by_pin` — so "pinned" means pinned in both senses rather
    // than a Z-order change the user can still shove around by accident.
    // `GlyphIcon::Pin` — MDI's pushpin glyph — is a literal fit for
    // "pinned above other windows". Persisted to `Settings::always_on_top`
    // the same way `click_through` is.
    let always_on_top_rect = egui::Rect::from_center_size(
        egui::pos2(
            click_through_rect.right() + TOGGLE_GAP + TOGGLE_ALWAYS_ON_TOP_SIDE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(TOGGLE_ALWAYS_ON_TOP_SIDE),
    );
    let always_on_top_label = if settings.always_on_top {
        "Always on top: on"
    } else {
        "Always on top: off"
    };
    if toggle_button(ui, always_on_top_rect, always_on_top_label, capturing, true).clicked() {
        settings.toggle_always_on_top();
        let level = if settings.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        let _ = tx_settings.send(settings.clone());
    }
    if let Some(pin) = icons.glyphs.get(GlyphIcon::Pin) {
        ui.painter().image(
            pin.id(),
            always_on_top_rect,
            UV_FULL,
            toggle_state_tint(settings.always_on_top),
        );
    }
}

/// Scans this frame's input events for the reply to a screenshot request
/// fired by the Share button (`toggle_cluster`'s
/// `ViewportCommand::Screenshot`) and hands the captured image to `write`.
/// Split out from the actual clipboard call so the routing — "a
/// `Screenshot` event reaches the writer" — is unit-testable with a fake
/// `write` and no live window or clipboard; the real call site
/// (`OverlayApp::ui`) passes `platform::write_clipboard_image`.
///
/// `write` takes the event's `Arc<ColorImage>` itself rather than a bare
/// `&ColorImage`: `egui::Event::Screenshot`'s `image` field is already an
/// `Arc`, and handing out the reference-counted handle (a cheap refcount
/// bump) instead of a borrow lets callers that need to keep the image past
/// this closure — `OverlayApp::ui` collects every landed image into a
/// `Vec` for the crop/write loop below — do so without a full pixel-buffer
/// deep copy (`epaint::ColorImage` derives `Clone` over `Vec<Color32>`,
/// which for a 4K capture is tens of MB).
///
/// A `Screenshot` event can only be `Event::Screenshot` in practice, but
/// nothing stops another viewport's reply from showing up in a multi-
/// viewport app; this app only ever has the root viewport, so every event
/// this frame is implicitly ours.
pub(crate) fn handle_screenshot_events(
    ctx: &egui::Context,
    mut write: impl FnMut(std::sync::Arc<egui::ColorImage>),
) {
    ctx.input(|input| {
        for event in &input.events {
            if let egui::Event::Screenshot { image, .. } = event {
                write(image.clone());
            }
        }
    });
}

/// Issue #96: the window-space y coordinate (in points) of the bottom of
/// the row list's actual content — top chrome plus only the populated rows
/// — used to crop the Share button's clipboard screenshot down to just
/// that content instead of the whole window.
///
/// `rows_area_height` is the row `ScrollArea`'s own allocated height: per
/// `draw_rows`'s `auto_shrink([false, false])` it always fills whatever
/// space the panel leaves it, so it is exactly what got rendered this
/// frame, regardless of how many rows there are. Clamping the rows' own
/// height to it handles both crop directions purely:
///
/// - Trailing empty space (fewer/shorter rows than the scroll area) is
///   trimmed — the bound stops right after the last row, not at the
///   scroll area's full height.
/// - Scrolled-off rows (more row content than the scroll area shows) are
///   never trimmed further than what was actually painted — the bound
///   clamps at the scroll area's height rather than the taller row-count
///   total, since content past that was clipped out of the render itself
///   and cannot be recovered from a screenshot of it.
pub(crate) fn rows_content_bottom_y(
    rows_top: f32,
    row_count: usize,
    row_height: f32,
    rows_area_height: f32,
) -> f32 {
    let rows_height = row_count as f32 * row_height;
    rows_top + rows_height.min(rows_area_height.max(0.0))
}

/// Converts a window-space y bound (points, from `rows_content_bottom_y`)
/// to a row count in the captured `ColorImage`'s physical-pixel coordinate
/// space, using the frame's `pixels_per_point` scale. Clamped to
/// `image_height_px` as a last-resort safety net — `pixels_per_point` is
/// still read fresh on the frame the reply lands on (see
/// `OverlayApp::pending_screenshot_bound`'s doc comment, which covers the
/// row-count half of this), so a DPI/zoom change mid-round-trip could in
/// principle still scale the bound past the image; this clamp makes sure
/// that only ever crops *less* than the full image, never indexes past its
/// end.
pub(crate) fn screenshot_crop_height_px(
    bottom_y_points: f32,
    pixels_per_point: f32,
    image_height_px: usize,
) -> usize {
    // Clippy's `neg_cmp_op_on_partial_ord`: `!(x > 0.0)` is not the same as
    // `x <= 0.0` once NaN is possible (NaN compares false either way, so
    // the negated form silently treats it as "not positive" too — which is
    // exactly the fallback wanted here, but spelled via `partial_cmp` to
    // make that NaN handling explicit rather than implicit in a negation).
    let is_positive = |v: f32| v.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater);
    if !is_positive(bottom_y_points) || !is_positive(pixels_per_point) {
        return 0;
    }
    let px = bottom_y_points * pixels_per_point;
    if !px.is_finite() {
        return image_height_px;
    }
    (px.round() as usize).min(image_height_px)
}

/// Crops a Share-button screenshot down to the header chrome plus
/// populated rows (issue #96), dropping trailing empty window background
/// below the last row — or keeping the full capture when the content
/// already fills it (scrolled-off rows, or the bound clamped past the
/// image). This is a pure post-capture crop of the `ColorImage` already in
/// hand: it never touches the live window's size or geometry, which is the
/// point of doing it here rather than by resizing the overlay — only the
/// clipboard image differs from a bare full-window capture.
///
/// Issue #82: a `crop_height` of `0` — from a non-positive bound or
/// `pixels_per_point`, per `screenshot_crop_height_px`'s own fallback —
/// also keeps the full image rather than producing a genuinely
/// zero-height `ColorImage`. arboard rejects a zero-height image outright
/// (`Error::ConversionFailure`), which is what made the Share button fail
/// on every single click; a full, uncropped screenshot is a far better
/// failure mode than no screenshot at all.
///
/// Takes and returns `Arc<ColorImage>`, not a bare `&ColorImage`/
/// `ColorImage`, so the `crop_height == 0` no-op path — which is exactly
/// the issue-#82 case above, not a rare edge — returns via a cheap Arc
/// refcount bump instead of a second full pixel-buffer deep copy on top of
/// the one `handle_screenshot_events`'s caller already avoids.
pub(crate) fn crop_screenshot_to_rows(
    image: &std::sync::Arc<egui::ColorImage>,
    bottom_y_points: f32,
    pixels_per_point: f32,
) -> std::sync::Arc<egui::ColorImage> {
    let [width, height] = image.size;
    let crop_height = screenshot_crop_height_px(bottom_y_points, pixels_per_point, height);
    if crop_height == 0 || crop_height >= height {
        return image.clone();
    }
    let pixels = image.pixels[..width * crop_height].to_vec();
    std::sync::Arc::new(egui::ColorImage::new([width, crop_height], pixels))
}

/// Issue #183: flattens a captured screenshot's alpha before the clipboard
/// write, so what lands on the clipboard is a visible image rather than a
/// mostly-invisible one.
///
/// The overlay is a transparent, borderless window: `PANEL_FILL` is painted
/// at `Settings::opacity` and the rounded corners are fully transparent, so
/// a `ViewportCommand::Screenshot` capture legitimately comes back with
/// translucent pixels. `epaint::Color32` stores those channels
/// *premultiplied*, while `arboard::ImageData` is straight (non-
/// premultiplied) RGBA, and on Windows arboard passes the alpha straight
/// through into the `CF_DIBV5` it writes. Pasted anywhere that honours that
/// alpha, the result is a near-invisible image — which from the outside is
/// indistinguishable from "the Copy button does nothing", with no error
/// anywhere for the log to catch.
///
/// Forcing every pixel opaque *is* compositing the capture over black,
/// precisely because the channels are already premultiplied: the RGB bytes
/// are correct as they stand and only the alpha byte changes. That is what
/// the overlay looks like over a dark game frame, and it pastes in every
/// clipboard consumer instead of none of them.
///
/// Returns the input `Arc` untouched when the capture is already fully
/// opaque — a refcount bump rather than a second full pixel-buffer copy,
/// same reasoning as `crop_screenshot_to_rows`'s no-op path.
pub(crate) fn flatten_screenshot_alpha(
    image: &std::sync::Arc<egui::ColorImage>,
) -> std::sync::Arc<egui::ColorImage> {
    if image.pixels.iter().all(|pixel| pixel.a() == u8::MAX) {
        return image.clone();
    }
    let pixels = image
        .pixels
        .iter()
        .map(|pixel| {
            egui::Color32::from_rgba_premultiplied(pixel.r(), pixel.g(), pixel.b(), u8::MAX)
        })
        .collect();
    std::sync::Arc::new(egui::ColorImage::new(image.size, pixels))
}

/// Decides what row bound a landed screenshot reply should crop to, and
/// what `OverlayApp::pending_screenshot_bound` must hold afterward.
///
/// Issue #96 (PR #98 review) `take()`s the pending bound so a stale value
/// can never be reused by a later, unrelated frame — but the original fix
/// took it *unconditionally*, once per frame, regardless of whether that
/// frame actually had a screenshot event to handle. `ViewportCommand::
/// Screenshot`'s reply is asynchronous and can land any number of frames
/// after the Share click that requested it, so the bound stashed on the
/// request frame was thrown away on the very next idle frame — long
/// before the reply could arrive. By reply time `pending_screenshot_bound`
/// was already `None`, `unwrap_or(0.0)` handed `crop_screenshot_to_rows` a
/// zero bound, and the resulting zero-height image made arboard reject
/// the clipboard write with `Error::ConversionFailure` on every attempt
/// (issue #82).
///
/// The fix: only consume (reset to `None`) the pending bound on the frame
/// that actually handles a screenshot event (`event_landed`); every other
/// frame must leave whatever is pending untouched so it survives however
/// many frames the round trip takes.
pub(crate) fn take_pending_screenshot_bound(
    pending: Option<f32>,
    event_landed: bool,
) -> (f32, Option<f32>) {
    if event_landed {
        (pending.unwrap_or(0.0), None)
    } else {
        (0.0, pending)
    }
}

/// Issue #156: the state transition behind `OverlayApp::screenshot_
/// capturing`, the guard `toggle_cluster`'s buttons read to decide whether
/// to suppress their hover fill and tooltip this frame.
///
/// `egui::ViewportCommand::Screenshot`'s own doc comment says it captures
/// "the next frame after" the one that sends it — not the request frame
/// itself — and its `Event::Screenshot` reply is asynchronous, landing any
/// number of frames later (the same round trip `take_pending_screenshot_
/// bound` accounts for). So the guard can't just be "true on the frame
/// `toggle_cluster` fires the request": it has to still be `true` on the
/// *next* frame (the one actually captured), and every frame after that
/// until the reply lands — otherwise a suppression that only covered the
/// click frame would be a silent no-op, since that frame is never the one
/// in the screenshot.
///
/// `requested_this_frame` is checked before `event_landed` so a new
/// request that happens to land in the same frame as an old capture's
/// reply keeps the guard set (a fresh capture is now in flight) rather
/// than clearing it.
pub(crate) fn screenshot_capture_guard(
    current: bool,
    requested_this_frame: bool,
    event_landed: bool,
) -> bool {
    if requested_this_frame {
        true
    } else if event_landed {
        false
    } else {
        current
    }
}

/// Issue #156: how many consecutive frames `screenshot_capturing` may stay
/// latched with no landed `Event::Screenshot` reply before `screenshot_
/// capture_timed_out` gives up on it and `OverlayApp::ui` forces the guard
/// closed anyway.
///
/// The reply can be silently dropped rather than merely slow: egui-wgpu's
/// `Painter::paint_and_update_textures` has several early-return paths — a
/// failed surface recreate, or its `render_state`/`surface_state` being
/// `None` — that return before `capture_data` ever reaches `read_screen_
/// rgba`, so the queued screenshot is dropped and no `Event::Screenshot` is
/// ever pushed. Without a bound, `screenshot_capture_guard` alone would
/// leave `screenshot_capturing` latched `true` for the rest of the
/// process, permanently suppressing the toggle cluster's hover fill and
/// tooltip on both buttons (they share the one flag) with no error and no
/// recovery short of restarting the app.
///
/// Counted in frames, so it needs frames to happen: since issue #349 the
/// overlay only repaints when something asks it to, and `OverlayApp::ui`
/// feeds `screenshot_capturing` into `repaint::RepaintInputs`'
/// `transient_timer_active` precisely so a pending capture holds the
/// `TRANSIENT_TIMER_REPAINT` (100 ms) cadence while it waits. At that
/// cadence this bound is about 2 seconds — comfortably longer than any
/// real `ViewportCommand::Screenshot` round trip, short enough that a
/// dropped reply doesn't leave the suppression visible for long.
pub(crate) const SCREENSHOT_CAPTURE_TIMEOUT_FRAMES: u32 = 20;

/// Issue #156: true once `screenshot_capture_frames_waited` has reached
/// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES` — see that constant's doc comment
/// for why the `Event::Screenshot` reply can be silently dropped and never
/// arrive at all. `OverlayApp::ui` feeds this straight into `screenshot_
/// capture_guard` as `event_landed`, so a timed-out wait clears the guard
/// through the exact same pure transition a real reply does, rather than a
/// second, separately-maintained clearing path.
pub(crate) fn screenshot_capture_timed_out(frames_waited: u32) -> bool {
    frames_waited >= SCREENSHOT_CAPTURE_TIMEOUT_FRAMES
}

/// Issue #156: the frame-count half of the timeout fallback — advances
/// `OverlayApp::screenshot_capture_frames_waited` alongside `screenshot_
/// capture_guard`, sharing the same two inputs so the two pure functions
/// can never fall out of step with each other.
///
/// Resets to `0` the instant a new request fires or a reply (real or
/// timed-out) lands — both already reset `screenshot_capturing` itself —
/// and increments on every other frame, i.e. every frame the wait for the
/// reply continues.
pub(crate) fn advance_screenshot_capture_wait(
    frames_waited: u32,
    requested_this_frame: bool,
    event_landed: bool,
) -> u32 {
    if requested_this_frame || event_landed {
        0
    } else {
        frames_waited + 1
    }
}

/// The Share round trip's per-frame sequencing: collect any `Event::
/// Screenshot` images that landed this frame, decide (via
/// `take_pending_screenshot_bound`) what row bound to crop them to and
/// what `pending_screenshot_bound` must hold afterward, and hand each
/// cropped image to `write`.
///
/// Extracted out of `OverlayApp::ui` — mirroring this file's existing
/// pure-helper-for-testability convention (see `handle_screenshot_events`)
/// — because this sequencing was previously exercised only by driving the
/// whole `ui()` method, which is infeasible to test directly: `eframe::
/// Frame` has only `pub(crate)` fields and no public constructor. This
/// logic never actually touches `_frame`, though — it needs only `ctx` and
/// `pending_screenshot_bound`, both of which a bare `egui::Context::
/// default()` and a plain `&mut Option<f32>` stand in for in a test, no
/// GPU or window required. `write` is the same seam `handle_screenshot_
/// events` uses, so a test can assert on what would have been written
/// without a real clipboard.
///
/// The screenshot images are collected first, outside `handle_screenshot_
/// events`'s `ctx.input` borrow, so the pending-bound take (which needs
/// `&mut pending_screenshot_bound`) and the crop/write loop can happen
/// afterward without a borrow conflict.
pub(crate) fn handle_share_screenshot(
    ctx: &egui::Context,
    pending_screenshot_bound: &mut Option<f32>,
    mut write: impl FnMut(std::sync::Arc<egui::ColorImage>),
) {
    let mut screenshot_images: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
    handle_screenshot_events(ctx, |image| screenshot_images.push(image));
    let event_landed = !screenshot_images.is_empty();
    let (rows_bottom_y, new_pending_screenshot_bound) =
        take_pending_screenshot_bound(*pending_screenshot_bound, event_landed);
    *pending_screenshot_bound = new_pending_screenshot_bound;
    if event_landed {
        let pixels_per_point = ctx.pixels_per_point();
        for image in &screenshot_images {
            let cropped = crop_screenshot_to_rows(image, rows_bottom_y, pixels_per_point);
            write(cropped);
        }
    }
}

/// Header title text (issue #9 slice 2; gated to boss fights by issue #42;
/// dungeon-final-boss precedence by issue #125; live-boss-first by issue
/// #131).
///
/// Precedence, highest first:
///
/// 1. The *live* boss, when the currently-selected target is a genuinely
///    recognized boss (`is_boss`): its resolved name, or `Monster #{id}`
///    when it's a recognized boss whose name didn't resolve (the two
///    vendored lists aren't guaranteed to agree — see
///    `EncounterInfo::is_boss`). This outranks `scene_boss_name` —
///    inverted from issue #125's original precedence — because a raid can
///    string several *different* final bosses together in one instance
///    (repo owner, issue #131), so once a second or third raid boss is
///    actually engaged, showing anything but the boss currently being
///    fought would be actively wrong. A single-final-boss dungeon is
///    unaffected: nothing else is ever `is_boss` there once the run's
///    target is the curated boss itself.
/// 2. else `scene_boss_name` — the current dungeon's final boss from the
///    curated `tables::SCENE_FINAL_BOSSES` (issue #201; learned at runtime
///    before that, issue #125/#131). This is what covers both "just walked
///    in, nothing engaged yet" (`boss_monster_id` is still `None`) and the
///    issue #125 case: `recompute_boss` selected a non-boss mid-dungeon
///    mech or add as `boss_uid` (`is_boss` false), so branch 1 doesn't
///    apply, but the header still shouldn't go blank or fall through to
///    "No target" when the dungeon's real boss is already known. Only
///    curated single-boss dungeons have an answer here; every other scene
///    falls through.
/// 3. else, with nothing engaged at all in a scene that offers a *choice*
///    of bosses (`multi_boss_scene`, issue #150): "Select a boss". Every
///    raid lets the party pick one of three bosses, and picking again after
///    a win or a wipe never leaves the scene — so this is the caption for
///    the whole time the party is standing in the instance with nothing
///    selected. Naming a boss there (branch 2 is suppressed for these
///    scenes precisely because the remembered one is a wrong guess two
///    times out of three) or calling it "No target" would both misdescribe
///    the moment. Branch 1 takes back over the instant *any* recognized
///    boss is engaged.
/// 4. else the pre-#125 fallback: blank for a non-boss pull with nothing
///    remembered, "No target" when nothing has been hit yet at all.
///
/// Always returns something (never omits the line) so `draw_header` can
/// render it unconditionally and the header's height never jitters between
/// frames depending on whether a target — or a name for it — is known.
///
/// "No target" is kept for the genuinely-empty-encounter case (no target at
/// all), but a non-boss pull with no remembered dungeon boss is a real
/// target we're deliberately not naming — showing `Monster #{id}` there was
/// dropped rather than kept as the non-boss fallback, since a raw id would
/// read as an unresolved boss name rather than the intentional omission it
/// actually is (the reference meter only names boss fights; see
/// `tables::is_boss_monster`). A *recognized boss* with no resolved name is
/// different: it's a real boss fight, and an empty header would be
/// indistinguishable from a trash pull — so that case still falls back to
/// the raw id.
///
/// The issue #131 limitation this used to carry — on entry to a multi-boss
/// raid, `scene_boss_name` naming whichever boss was latched last rather
/// than the one the party will actually fight — is what issue #150 fixed,
/// by having `Meter::snapshot` suppress that fallback for such a scene and
/// set `multi_boss_scene` instead (issue #201's curated table is
/// single-boss dungeons only, so it cannot reintroduce it either). Branch 3
/// is the resulting caption.
///
/// While a finished fight is being held on screen, every field here
/// describes *that* fight rather than live state (issue #152), so nothing
/// in this precedence has to know about zoning: the header keeps naming the
/// boss whose frozen numbers are on the rows below it.
///
/// Also called by `pipeline::record_fight_end` so a saved encounter stores
/// the *same* label the live header showed (issue #39, spec DECISION D2).
pub(crate) fn encounter_title(e: &EncounterInfo) -> String {
    if e.is_boss {
        // `is_boss` is only ever true alongside a `Some` `boss_monster_id`
        // (see `Meter::snapshot`), so the `None` arm here is unreachable in
        // practice — kept only so this stays a total match rather than an
        // `unwrap`.
        return match e.boss_monster_id {
            Some(id) => e
                .boss_name
                .map(str::to_string)
                .unwrap_or_else(|| format!("Monster #{id}")),
            None => String::new(),
        };
    }
    if let Some(name) = e.scene_boss_name {
        return name.to_string();
    }
    match e.boss_monster_id {
        None if e.multi_boss_scene => "Select a boss".to_string(),
        None => "No target".to_string(),
        Some(_) => String::new(),
    }
}

/// Header subtitle text (issue #9 slice 2): the scene name when known, else
/// its raw scene id, else `None` — `draw_header` paints the subtitle row
/// blank in that case. The row's space is reserved either way (issue #91,
/// `header_text_band_height`); only its ink is conditional.
///
/// Also called by `pipeline::record_fight_end` so a saved encounter stores
/// the *same* label the live header showed (issue #39, spec DECISION D2).
pub(crate) fn encounter_subtitle(e: &EncounterInfo) -> Option<String> {
    match (e.scene_name, e.scene_id) {
        (Some(name), _) => Some(name.to_string()),
        (None, Some(id)) => Some(format!("Scene #{id}")),
        (None, None) => None,
    }
}

/// Height of the header's title line, reused by both `draw_header`'s
/// drag-band sizing and `default_inner_height` so they can't drift apart —
/// the same pattern `ROW_HEIGHT` follows for player rows.
/// Issue #91 raised this from `20.0`: measured against
/// `docs/reference/new-shinra-ex.webp`, our boss name sat with barely a
/// point of air under its descenders, while the reference leaves the name a
/// visibly taller line box.
pub(crate) const TITLE_LINE_HEIGHT: f32 = 22.0;

/// White the title line is painted in — the source's inherited `White`
/// title foreground, deliberately not `ui.visuals().text_color()` (the
/// theme's default, dimmer body-text white) since the title needs to read
/// as the visually heaviest element in the header. `draw_title_line` is its
/// only user: the header stat pills, which once shared it, are painted a
/// step dimmer in `PILL_VALUE_COLOR` so the title still outweighs them.
pub(crate) const TITLE_TEXT_COLOR: egui::Color32 = egui::Color32::WHITE;

/// Height of the header's subtitle line, always reserved by the header band
/// whether or not there is an area name to paint into it (issue #91, see
/// `header_text_band_height`). Still not part of `default_inner_height`,
/// which keeps the opening size it was measured at (see its doc).
///
/// `TITLE_LINE_HEIGHT (22) + ITEM_SPACING_Y (2) + SUBTITLE_LINE_HEIGHT (16)
/// == 40.0`. Issue #91 grew both line heights off the source's original
/// `Height="36"` grid (`20 + 2 + 14`): pixel-measured against
/// `docs/reference/new-shinra-ex.webp`, the reference's area name clears its
/// own descenders and the separator above it with more room than a 14pt line
/// box leaves.
pub(crate) const SUBTITLE_LINE_HEIGHT: f32 = 16.0;

/// Subtitle text color — the source's `#5fff`, white at ~1/3 alpha.
pub(crate) const SUBTITLE_TEXT_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x55);

// -- header text gutter (issue #59, #62) --------------------------------
//
// The reference render tabs the boss name and dungeon name in from the
// window's left edge, leaving a gutter that holds the emblem (see
// `HEADER_EMBLEM_SIZE` below) and the accent separator stroke sweeping
// right out of it under the title.

/// Left gutter the header emblem occupies, in points: the source's 60pt
/// `Svg.HPBar` at `Margin="-26 0 0 -8"`, i.e. `60 - 26 = 34` visible points
/// before the title column starts. A fixed width, not a fraction of the
/// window: in the source the emblem is a fixed-size `Path` in an `Auto`
/// column, so the gutter does not breathe with the window.
pub(crate) const HEADER_GUTTER_WIDTH: f32 = 34.0;
/// The source title/subtitle `Margin="2 … 0 0"` — a hair of air between the
/// gutter and the text.
pub(crate) const HEADER_TEXT_PAD_X: f32 = 2.0;

/// Width reserved at the *right* end of the title/subtitle rows — the
/// source's `ComboBoxToggleButton` chevron column, `Width="32"`.
///
/// Issue #54's collapse chevron is what occupies that strip — `chevron_rect`
/// centers its box in exactly this width, on the title row.
pub(crate) const HEADER_RIGHT_CONTROL_WIDTH: f32 = 32.0;

/// The sub-rect of a header row that title/subtitle text may actually paint
/// into: indented on the left by the fixed `HEADER_GUTTER_WIDTH` +
/// `HEADER_TEXT_PAD_X`, and stopping short of the right edge by
/// `HEADER_RIGHT_CONTROL_WIDTH`. Never inverted — at an absurdly narrow
/// width the right edge collapses onto the left one, giving an empty (not
/// negative) rect, which clips the text away entirely rather than painting
/// it backwards.
pub(crate) fn header_text_rect(row: egui::Rect) -> egui::Rect {
    header_text_rect_reserving(row, HEADER_RIGHT_CONTROL_WIDTH)
}

/// The sub-rect the *title* row's text may paint into (issue #185): the same
/// geometry as `header_text_rect`, but reserving `TITLE_RIGHT_CONTROLS_
/// WIDTH` — the chevron strip *plus* the toggle pill that now sits left of
/// it — instead of the chevron strip alone. The subtitle row keeps
/// `header_text_rect`, since the pill is on the title row only.
pub(crate) fn title_text_rect(row: egui::Rect) -> egui::Rect {
    header_text_rect_reserving(row, TITLE_RIGHT_CONTROLS_WIDTH)
}

/// The shared body of `header_text_rect` and `title_text_rect`: the two
/// differ only in how much of the row's right end is reserved for controls,
/// and every degradation rule (never inverted, clamped against the left
/// edge) is identical, so it is spelled once here rather than twice.
pub(crate) fn header_text_rect_reserving(row: egui::Rect, right_reserve: f32) -> egui::Rect {
    let left = row.left() + HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;
    let right = (row.right() - right_reserve).max(left);
    egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()))
}

/// Gap between the header's text band (title + subtitle) and the
/// stat-pill row below it. Its own constant rather than the `ITEM_SPACING_Y`
/// (2.0) every other adjacent pair pays, because the stat row has to clear
/// the subtitle's *descenders* the way the reference does: measured on
/// `docs/reference/new-shinra-ex.webp`, the reference leaves 14px between the
/// subtitle's ink and the stat row's ink, where ours left 8px and the pills
/// crowded the area name. The extra 4pt here is what buys the difference
/// without inflating every other gap in the window.
///
/// `draw_header` applies it explicitly (`ui.add_space`) *and* budgets it in
/// `header_band_height`, so the drag band, the wash and the painted rows all
/// stay derived from this one number.
pub(crate) const HEADER_STAT_ROW_GAP: f32 = 6.0;

/// How far the stat row (timer, DPS, damage, toggle cluster) is inset from
/// the panel's left content edge. The timer used to sit flush against it, so
/// its capsule was cropped by the window border and drawn as a half-pill to
/// match; inset, it can wear a whole oval (see `StatPill::timer`).
///
/// Exactly the title column's own indent, not a margin of its own:
/// `header_text_rect` starts the boss name and area name at this same sum,
/// so the timer reads as sitting *under* the title rather than tucked into
/// the gutter beside it.
///
/// It is also what keeps the gutter emblem off the timer. That mark's box
/// ends at `HEADER_EMBLEM_LEFT_BLEED + HEADER_EMBLEM_SIZE ==
/// HEADER_GUTTER_WIDTH` and hangs 14pt into this row's vertical span, so
/// this sum leaves `HEADER_TEXT_PAD_X` of daylight between the mark's right
/// edge and the row's leftmost ink — which is why the mark no longer has to
/// be clipped to the text band (and have its bottom corner sliced off) to
/// stay clear of the readout. See `header_emblem_rect`.
pub(crate) const HEADER_STAT_ROW_INSET_X: f32 = HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;

/// Height of `draw_header`'s drag band: the title line, the subtitle line,
/// and the button row (`button_row_height`, egui's `interact_size.y`), plus
/// the gaps egui's vertical layout stacks between them — `ITEM_SPACING_Y`
/// between the title and subtitle rows, and `HEADER_STAT_ROW_GAP` above the
/// stat-pill row. Extracted from `draw_header` so it is unit-testable
/// without a live `egui::Ui`.
///
/// A constant `68.0` at the real `BUTTON_ROW_HEIGHT`, with no dependence on
/// whether an area name is known — see `header_text_band_height`.
/// The header band height the window-sizing math should use: the height
/// `draw_header` *measured* on the last painted frame, or
/// `header_band_height`'s constant budget when there is no measurement yet.
///
/// Issue #340: every consumer outside `draw_header` used to re-derive the
/// band from `header_band_height(BUTTON_ROW_HEIGHT)`, i.e. from two
/// constants that only agree with the painted header as long as nothing
/// (a restyled `Spacing::interact_size`, a taller text row, a future
/// header element) changes what the header actually occupies. The header
/// now records the rect it really painted once per frame
/// (`OverlayApp::header_rect`), and this is the single place that turns
/// that measurement — or its absence, on the very first frame — into the
/// number those consumers size against.
pub(crate) fn measured_header_band_height(header_rect: Option<egui::Rect>) -> f32 {
    header_rect.map_or_else(
        || header_band_height(BUTTON_ROW_HEIGHT),
        |rect| rect.height(),
    )
}

pub(crate) fn header_band_height(button_row_height: f32) -> f32 {
    header_text_band_height() + HEADER_STAT_ROW_GAP + button_row_height
}

/// Issue #158 (corrected by issue #297): the panel-top-relative y where the
/// first player row actually begins — which is *not* `band_height`.
/// `OverlayApp::ui` puts a `ui.separator()` (`SEPARATOR_HEIGHT`, egui's own
/// fixed 6.0) between the header and the row list, and egui's vertical
/// layout pays its ordinary `ITEM_SPACING_Y` gap *twice* around it: once
/// between the header's last widget and the separator, and again between
/// the separator and the first row — `egui::Ui::cursor`'s own doc comment
/// is explicit that the cursor always sits one `item_spacing` past the
/// latest child, so placing the separator consumes the first gap and its
/// own advance opens the second. So the band's own bottom edge is
/// `2 * ITEM_SPACING_Y + SEPARATOR_HEIGHT` short of where the rows start,
/// not `ITEM_SPACING_Y + SEPARATOR_HEIGHT` (issue #297: the row backdrop
/// image painted at the old, one-gap offset left a bare sliver of panel
/// fill between it and the header wash — flush for a same-colored solid
/// wash, but a visible seam once either region carries its own artwork).
/// This is the single function both `default_inner_height` (the window's
/// default open height) and the header wash (`draw_header`'s
/// `wash_height`) derive the true offset from, so the two can never drift
/// back out of sync the way `band_height` alone did. Verified against a
/// real `egui::Ui` — not just self-consistency — by
/// `the_row_area_begins_exactly_where_first_player_row_top_offset_predicts`.
pub(crate) fn first_player_row_top_offset(band_height: f32) -> f32 {
    band_height + 2.0 * ITEM_SPACING_Y + SEPARATOR_HEIGHT
}

/// Height of the header's *text* rows alone: the title line, the gap, and
/// the subtitle line under it (issue #91's `22 + 2 + 16`, grown from the
/// source's `Height="36"` grid).
///
/// Unconditional. The subtitle's line and gap are reserved whether or not a
/// scene is known, so the header is a fixed-height band: the app's idle
/// "No target" state used to skip them, which collapsed the band from 68 to
/// 50 and jumped the whole stat-pill row 18pt up the window the moment the
/// area name arrived or went away. `draw_header` renders the subtitle row
/// empty rather than omitting it (see
/// `a_missing_area_name_does_not_collapse_the_header_or_lift_the_stat_row`).
///
/// This, not the whole drag band, is the block the left gutter belongs to:
/// `header_text_rect` indents these rows by `HEADER_GUTTER_WIDTH` to leave
/// room for the emblem, which is why the emblem's box is *centered* on this
/// height (issue #75) rather than on the band.
///
/// Centered on, no longer clipped to. Issue #75 did both; the clip cut the
/// mark's bottom corner off, and issue #91 replaced it with horizontal
/// clearance instead (`HEADER_STAT_ROW_INSET_X`, which now starts the stat
/// row right of the emblem rather than under it). `draw_header` clips the
/// mark to the whole band.
///
/// The background wash is deliberately *not* bounded by this either (issue
/// #91): it spans the whole `header_band_height`, stat row included.
pub(crate) fn header_text_band_height() -> f32 {
    TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT
}

/// Paints the header's title line (boss name/id/placeholder) at a fixed
/// height so `draw_header`'s drag band and `default_inner_height` can both
/// reason about it exactly, the same way `draw_row` paints stat text inside
/// an `allocate_exact_size`d rect instead of an auto-sized `ui.label`.
///
/// Returns the *whole allocated row* rect, from which `draw_header` derives
/// every other thing it paints inside it without allocating any extra
/// vertical space: the gutter emblem (`header_emblem_rect`, issue #59), the
/// accent separator (`title_separator_segments` over `title_separator_rect`,
/// issue #62), and the collapse chevron (`chevron_rect`, issue #54), which
/// sits in the reserved strip at the row's right end. The row rather than
/// the text rect, because the text rect has the chevron's own strip already
/// cut off it.
///
/// The title's paint is clipped to the text rect, so an overlong boss name
/// loses its tail instead of running into that strip.
pub(crate) fn draw_title_line(ui: &mut egui::Ui, text: &str) -> egui::Rect {
    let desired_size = egui::vec2(ui.available_width(), TITLE_LINE_HEIGHT);
    let (row, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    // `title_text_rect`, not `header_text_rect`: the title row reserves the
    // toggle pill's width as well as the chevron's (issue #185).
    let rect = title_text_rect(row);
    paint_bold_text(
        &ui.painter().with_clip_rect(rect),
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        FONT_SIZE_TITLE,
        TITLE_TEXT_COLOR,
    );
    row
}

// -- header gutter emblem (issue #59) ------------------------------------

/// The source's `Svg.HPBar` beside the encounter name: 60x60, bled off the
/// left edge by `Margin="-26 0 0 -8"`, so only its right two-thirds are ever
/// on screen. Vertically it is centered on the header's *text* band but
/// clipped to the whole header band — see `header_emblem_rect`.
pub(crate) const HEADER_EMBLEM_SIZE: f32 = 60.0;
/// The `Margin`'s left component: the emblem hangs 26pt off the left edge of
/// the header rows, so only its right `60 - 26 = 34`pt — one
/// `HEADER_GUTTER_WIDTH` — is ever on screen.
pub(crate) const HEADER_EMBLEM_LEFT_BLEED: f32 = -26.0;
/// The `Margin`'s bottom component (`-8`): a *negative* bottom margin, which
/// in WPF adds to the height the emblem is centered in rather than moving
/// it. With the source's 36pt header grid that gives `(36 + 8 - 60)/2 = -8`,
/// i.e. a top edge 8pt above the grid. Our text band is 40pt rather than 36
/// (issue #91 grew the two line heights), so `header_emblem_rect` recomputes
/// the centering from the text band it is actually given instead of baking
/// the source's one case in.
pub(crate) const HEADER_EMBLEM_BOTTOM_BLEED: f32 = 8.0;
/// `Fill="SlateGray"`. The source's gutter placement
/// (`DamageMeter.UI/HUD/Controls/MainView.xaml`, the `Width="60" Height="60"
/// Margin="-26 0 0 -8"` `Path` that `header_emblem_rect`'s constants are
/// measured off) carries no `Opacity` attribute of its own — only the wash's
/// separate blown-up copy does (`Opacity=".05"`, already encoded as
/// `HEADER_WASH_EMBLEM_COLOR`'s alpha `13`). So there is no reference number
/// to port here (issue #252); `0x80` (half alpha) is a chosen value, not a
/// measured one — it matches this module's own established "dimmed but
/// legible" idiom (`TOOLBAR_ICON_TINT`'s half-white) rather than painting
/// the mark fully opaque as a bare `Color32::from_rgb` implied.
pub(crate) const HEADER_EMBLEM_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x70, 0x80, 0x90, 0x80);

/// Where the header emblem's 60x60 box sits: bled off the left of the title
/// row `row`, and vertically centered on the header's text band
/// (`text_band_height`, plus the source's negative bottom margin) that
/// starts at that row's top. Pure geometry, so the negative-margin bleed is
/// unit-testable without a painter.
///
/// **The off-centre placement here is deliberate — do not "fix" it.** It has
/// been squared up to the band once (issue #91) and reverted once.
/// `emblem.png` is a 512x512 canvas holding one perfect diamond whose
/// corners are the midpoints of the canvas' edges, so a box of exactly
/// `header_band_height` *does* make the diamond symmetric about the band —
/// and that is not what this mark is. It decorates the title and area-name
/// rows, the ones `header_text_rect` indents by `HEADER_GUTTER_WIDTH` to
/// make room for it, so it is centered on those rows (plus the source's
/// negative bottom margin) and rides high in the band: top edge 6pt above
/// the panel, bottom edge 14pt below the text band. Both numbers are the
/// design.
///
/// What is *no longer* true is that a clip is what keeps its lower blade
/// off the timer. Issue #75 clipped this to the text band for that, and the
/// clip landed at 40 while the box reaches 54 — it sliced the diamond's
/// bottom corner clean off, which is the bug that reading closes. The stat
/// row moved right instead
/// (`HEADER_STAT_ROW_INSET_X == HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X`),
/// clearing the emblem's right edge at `HEADER_GUTTER_WIDTH` by
/// `HEADER_TEXT_PAD_X`; that horizontal daylight is the whole separation
/// now, and `draw_header` clips the mark to the header band, which cuts
/// nothing off the bottom. The mark's *top* corner is still cut — by the
/// panel's own top edge, since the box starts above it. That is the
/// asymmetry, and it stays.
///
/// This bounds the *gutter* mark only. The background wash
/// (`header_wash_rect` / `draw_header_wash`) is a different rect and also
/// spans the whole header band, stat-pill row included.
pub(crate) fn header_emblem_rect(row: egui::Rect, text_band_height: f32) -> egui::Rect {
    let available = text_band_height + HEADER_EMBLEM_BOTTOM_BLEED;
    egui::Rect::from_min_size(
        egui::pos2(
            row.left() + HEADER_EMBLEM_LEFT_BLEED,
            row.top() + (available - HEADER_EMBLEM_SIZE) / 2.0,
        ),
        egui::Vec2::splat(HEADER_EMBLEM_SIZE),
    )
}

// -- header background wash (issue #59, #62) -----------------------------

/// The decorative panel behind the header rows: a diagonal SlateGray
/// gradient (`#50708090` -> transparent) with a very faint, oversized
/// `Svg.HPBar` bleeding off its right edge. The source additionally applies
/// a vertical `OpacityMask` (white -> transparent at .9); egui has no
/// opacity masks, and the diagonal gradient already falls to zero by the
/// bottom-right, so the mask is deliberately not reproduced.
///
/// Issue #81 replaced a fixed `98.0`pt run — taller than the drag band
/// itself, so its tail bled into the player rows — with a height derived
/// from the content. Issue #91 settles which content: the whole header band
/// (`header_band_height`), stat-pill row included. The gradient and the
/// oversized emblem share one rect, so both now run the full band. Issue
/// #91 believed that made both flush with the first player row; issue #158
/// found the band's own bottom edge is actually 8pt short of it (the
/// `ui.separator()` between the header and the rows, plus the layout's
/// `ITEM_SPACING_Y` gap before it, are both outside the band) and extended
/// the wash past the band to `first_player_row_top_offset` so it now really
/// does stop flush with the first player row. No fixed constant is left to
/// drift out of sync with the content it sits behind.
/// Inset from the panel's edges the wash is painted at, so its square
/// corners never poke past the panel's own `PANEL_CORNER_RADIUS`-rounded,
/// `PANEL_BORDER_WIDTH`-thick border.
pub(crate) const HEADER_WASH_INSET: f32 = 1.0;
/// Alpha at the wash gradient's brightest (top-left) stop — `Opacity=".5"`.
pub(crate) const HEADER_WASH_TOP_ALPHA: u8 = 0x50;
/// Side of the wash's oversized `Svg.HPBar` box, in points — the same emblem
/// the gutter draws at `HEADER_EMBLEM_SIZE`, blown up as wallpaper.
pub(crate) const HEADER_WASH_EMBLEM_SIZE: f32 = 200.0;
/// How far the wash emblem's right edge overhangs the wash's own right edge,
/// in points: the source right-aligns the wash `Svg.HPBar` with a `-25` right
/// margin, so its last 25pt hang off the panel and the wash's clip rect cuts
/// them away — the mirror of the gutter emblem's `HEADER_EMBLEM_LEFT_BLEED`.
///
/// Nudged in from the source's literal `25` to `17` (issue #255's
/// live-window pass): at `25` the emblem's circular arc edge sat almost
/// dead-center under the title row's toggle pill (`title_toggle_pill_rect`),
/// cutting through the click-through glyph's box and locally lifting the
/// background behind it. Shrinking the overhang slides the whole square —
/// and the visible arc inside it — further from the panel's right edge,
/// clearing the toggle glyph boxes without touching the wash's size, alpha
/// or the toggle cluster's own layout.
pub(crate) const HEADER_WASH_EMBLEM_BLEED: f32 = 17.0;
/// `Opacity=".05"` on a SlateGray fill.
pub(crate) const HEADER_WASH_EMBLEM_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x70, 0x80, 0x90, 13);

/// Where the wash panel sits for a central panel of `panel`: inset from the
/// panel's left, top and right edges by `HEADER_WASH_INSET`, and running down
/// `height` points rather than to the panel's bottom. Pure geometry, so the
/// inset is unit-testable without a painter — the same factoring as
/// `header_emblem_rect`. `height` is the caller's to pick (`draw_header`
/// passes `header_band_height - HEADER_WASH_INSET`, issue #91) rather than a
/// fixed constant here, so the wash can never outgrow the band it decorates.
pub(crate) fn header_wash_rect(panel: egui::Rect, height: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        panel.min + egui::Vec2::splat(HEADER_WASH_INSET),
        egui::vec2(panel.width() - 2.0 * HEADER_WASH_INSET, height),
    )
}

/// Where the wash's oversized emblem sits inside a wash of `wash`: vertically
/// centered on it and right-aligned so exactly `HEADER_WASH_EMBLEM_BLEED`
/// points overhang its right edge. Taller than the wash as well as wider, so
/// both the overhang and the top/bottom overflow rely on the caller's clip
/// rect.
pub(crate) fn header_wash_emblem_rect(wash: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            wash.right() + HEADER_WASH_EMBLEM_BLEED - HEADER_WASH_EMBLEM_SIZE,
            wash.center().y - HEADER_WASH_EMBLEM_SIZE / 2.0,
        ),
        egui::Vec2::splat(HEADER_WASH_EMBLEM_SIZE),
    )
}

/// Paints the header's decorative background wash — a diagonal gradient
/// panel with a huge, nearly-invisible emblem bleeding off its right edge —
/// clipped to its own rect so it can never bleed into the rows below or over
/// the panel's rounded corners. `panel` is the whole central panel's rect
/// (not the drag band); `height` (issue #158, `first_player_row_top_offset`
/// of `header_band_height` less `HEADER_WASH_INSET`) is what actually
/// bounds the wash — the whole header band plus the separator gap below
/// it, stopping exactly where the first player row begins
/// (`wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row`).
///
/// The source rounds the wash's top corners (`CornerRadius="7 7 0 0"`); egui
/// cannot clip to a rounded rect this cheaply, so the wash keeps square
/// corners — at alpha `0x50` under the panel's own 8pt-rounded, 1pt border
/// the difference is sub-pixel.
///
/// Issue #121: when `settings.header_image` names a loadable file, that
/// image replaces both layers below — the gradient *and* the oversized
/// emblem — cover-cropped to this same rect and painted at
/// `settings.opacity`, with the legibility scrim over it. It replaces the
/// wash only; the small gutter emblem beside the title (`header_emblem_rect`,
/// painted by `draw_header` after this returns) is a foreground mark on the
/// title rows rather than background artwork, and the title's own indent is
/// sized for it, so it stays. Anything that fails to load falls straight
/// through to the default artwork below.
///
/// That default artwork fades with the same `settings.opacity` (issue #252):
/// every fill and the emblem image is `.gamma_multiply`'d by it, the same
/// pattern `PANEL_FILL`/`PANEL_BORDER_COLOR` use at the `Frame` level and
/// the skill window uses throughout (issue #184), so the wash tracks the
/// rest of the window's chrome instead of staying at its fixed baked-in
/// alpha — and so the two paths agree on what the slider means.
pub(crate) fn draw_header_wash(
    ui: &egui::Ui,
    panel: egui::Rect,
    icons: &Icons,
    height: f32,
    settings: &Settings,
) {
    let wash_rect = header_wash_rect(panel, height);
    let painter = ui.painter().with_clip_rect(wash_rect);

    if paint_background_image(&painter, icons, ImageSlot::Header, settings, wash_rect) {
        return;
    }

    // Issue #252: the default artwork fades with the same slider the header
    // image above is already painted at.
    let opacity = Opacity::new(settings.opacity);

    // Top-left brightest, fading to zero at the bottom-right — the source's
    // `LinearGradientBrush` with no explicit start/end points defaults to
    // that diagonal.
    let slate = |a: u8| opacity.apply(egui::Color32::from_rgba_unmultiplied(0x70, 0x80, 0x90, a));
    let mid_alpha = HEADER_WASH_TOP_ALPHA / 2;
    painter.add(egui::Shape::mesh(gradient_mesh(
        wash_rect,
        slate(HEADER_WASH_TOP_ALPHA),
        slate(mid_alpha),
        slate(mid_alpha),
        slate(0),
    )));

    if let Some(emblem) = icons.glyphs.get(GlyphIcon::Emblem) {
        let emblem_rect = header_wash_emblem_rect(wash_rect);
        painter.image(
            emblem.id(),
            emblem_rect,
            UV_FULL,
            opacity.apply(HEADER_WASH_EMBLEM_COLOR),
        );
    }
}

// -- row-list backdrop (issue #253) --------------------------------------

/// Where the user's row-list backdrop is painted, for a row area of
/// `available` inside a central panel of `panel`.
///
/// `available` is what `OverlayApp::ui`'s layout cursor has left once the
/// header band and its separator are behind it — i.e. exactly the strip
/// `draw_rows`/`draw_history` are about to fill — so the backdrop always
/// starts flush under the header wash and never overlaps it, whatever the
/// header's height works out to this frame.
///
/// The intersection with the inset panel is what keeps the image's square
/// corners off the panel's own `PANEL_CORNER_RADIUS`-rounded,
/// `PANEL_BORDER_WIDTH`-thick border along the bottom and sides — the same
/// job, and the same `HEADER_WASH_INSET`, that `header_wash_rect` does at
/// the top. Pure geometry, so both properties are unit-testable without a
/// painter, the same factoring as `header_wash_rect`/`header_emblem_rect`.
pub(crate) fn row_backdrop_rect(available: egui::Rect, panel: egui::Rect) -> egui::Rect {
    available.intersect(panel.shrink(HEADER_WASH_INSET))
}

/// Paints the user's backdrop image behind the player-row list (issue
/// #253), or nothing at all when none is configured or it failed to load —
/// in which case the panel's own `PANEL_FILL` shows through exactly as it
/// always has.
///
/// Called from `OverlayApp::ui` *before* `draw_rows`/`draw_history` rather
/// than from inside them, which is what puts it behind every row: egui
/// paints in call order, so the row hover fills, the accent lines, the
/// share bar and all the text land on top of this without any of them
/// needing to know it exists. That also means it costs nothing on the
/// (default) path where no image is configured, and it needs no change to
/// `draw_rows`' signature or its half-dozen call sites.
///
/// The scrim `paint_background_image` lays over the image is what keeps the
/// rows legible over arbitrary artwork — see `BACKGROUND_IMAGE_SCRIM_ALPHA`.
pub(crate) fn draw_row_backdrop(
    ui: &egui::Ui,
    panel: egui::Rect,
    available: egui::Rect,
    icons: &Icons,
    settings: &Settings,
) {
    let rect = row_backdrop_rect(available, panel);
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect);
    paint_background_image(&painter, icons, ImageSlot::Backdrop, settings, rect);
}

/// Color of the fading separator line painted under the header title
/// (`title_separator_segments`) — the source's `#708090`.
pub(crate) const TITLE_SEPARATOR_RGB: (u8, u8, u8) = (0x70, 0x80, 0x90);

/// Alpha the separator starts at, at its left (indented) end — the source's
/// left stop is a fully opaque `#708090`.
pub(crate) const TITLE_SEPARATOR_MAX_ALPHA: u8 = 255;

/// Thickness, in points, of the title separator line — `StrokeThickness="2"`.
pub(crate) const TITLE_SEPARATOR_THICKNESS: f32 = 2.0;

/// The source's `Margin="-5 7.5 32 0"`: only the `-5` left bleed and the `32`
/// right reserve survive from that margin (as `TITLE_SEPARATOR_LEFT_BLEED`
/// and `HEADER_RIGHT_CONTROL_WIDTH` respectively). The margin's `7.5` is
/// measured from the WPF source's own container, not from our title row, so
/// taking it literally (as an earlier version of this constant did) put a
/// 2pt opaque stroke at `row.top()+7.5..+9.5` — through the middle of the
/// 13pt title glyphs. Pixel-measured against the reference render, the
/// stroke actually sits in the gap between the title and subtitle rows: 5pt
/// below the title baseline and ~5pt above the subtitle's cap-top, which for
/// our geometry is exactly the title row's bottom edge (see
/// `title_separator_rect`), inside the `ITEM_SPACING_Y` gap egui's vertical
/// layout already leaves there.
pub(crate) const TITLE_SEPARATOR_LEFT_BLEED: f32 = 5.0;

/// Number of thin strips `title_separator_segments` divides the fade into.
/// High enough to read as a smooth gradient, modest enough to stay cheap to
/// paint every frame.
pub(crate) const TITLE_SEPARATOR_SEGMENTS: usize = 24;

/// The rect the fading title separator is painted over, for a title row
/// `title_row`: it bleeds `TITLE_SEPARATOR_LEFT_BLEED` back into the gutter
/// from the title's own left edge and clears the chevron's reserved strip on
/// the right, sitting flush against the title row's bottom edge — the gap
/// between the title and subtitle rows in the reference render (see the
/// `TITLE_SEPARATOR_LEFT_BLEED` doc comment for why this isn't the source
/// margin's literal `7.5`).
pub(crate) fn title_separator_rect(title_row: egui::Rect) -> egui::Rect {
    let left = title_row.left() + HEADER_GUTTER_WIDTH - TITLE_SEPARATOR_LEFT_BLEED;
    let right = (title_row.right() - HEADER_RIGHT_CONTROL_WIDTH).max(left);
    let top = title_row.bottom();
    egui::Rect::from_min_max(
        egui::pos2(left, top),
        egui::pos2(right, top + TITLE_SEPARATOR_THICKNESS),
    )
}

/// Builds the fading title-underline as a series of thin filled rects:
/// egui has no built-in gradient stroke, so the "sweeps out of the gutter
/// and fades away to the right" stroke from the reference render is
/// approximated with segments whose alpha steps down linearly from
/// `TITLE_SEPARATOR_MAX_ALPHA` at `rect`'s left edge to zero at its right
/// one. `rect` is `title_separator_rect`'s output, so the stroke starts
/// where the source's does — bled back into the gutter — and runs the width
/// of the title rather than stopping at its midpoint. Extracted as a pure
/// function, same reasoning as `share_bar_paints`: unit-testable without a
/// live `egui::Ui`.
pub(crate) fn title_separator_segments(rect: egui::Rect) -> Vec<(egui::Rect, egui::Color32)> {
    let (r, g, b) = TITLE_SEPARATOR_RGB;
    let segment_width = rect.width() / TITLE_SEPARATOR_SEGMENTS as f32;
    let y = rect.top();

    (0..TITLE_SEPARATOR_SEGMENTS)
        .map(|i| {
            let t = i as f32 / (TITLE_SEPARATOR_SEGMENTS - 1) as f32; // 0.0 ..= 1.0
            let alpha = ((1.0 - t) * TITLE_SEPARATOR_MAX_ALPHA as f32).round() as u8;
            let x0 = rect.left() + i as f32 * segment_width;
            let segment_rect = egui::Rect::from_min_size(
                egui::pos2(x0, y),
                egui::vec2(segment_width, TITLE_SEPARATOR_THICKNESS),
            );
            (
                segment_rect,
                egui::Color32::from_rgba_unmultiplied(r, g, b, alpha),
            )
        })
        .collect()
}

/// Paints the header's subtitle line (scene name/id), dimmed. Always
/// called, with empty text when `encounter_subtitle` returned `None`
/// (issue #91): the row's height is part of the header's fixed band, so it
/// is reserved — and rendered blank, with no placeholder — rather than
/// skipped, which would let the stat row below it ride up.
pub(crate) fn draw_subtitle_line(ui: &mut egui::Ui, text: &str) {
    let desired_size = egui::vec2(ui.available_width(), SUBTITLE_LINE_HEIGHT);
    let (row, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    // Same indent and same right-hand reserve as the title (issue #56): the
    // reference lines the dungeon name up under the boss name exactly.
    let rect = header_text_rect(row);
    ui.painter().with_clip_rect(rect).text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        regular(FONT_SIZE_SUBTITLE),
        SUBTITLE_TEXT_COLOR,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
    /// Issue #91: the timer is inset from the panel's left content edge by
    /// `HEADER_STAT_ROW_INSET_X` rather than sitting flush against the
    /// window border the way its old half-pill did. That gap is the whole
    /// reason the timer may keep a *full* oval — nothing crops it.
    ///
    /// Measured on the capsule itself, not on the duration's glyphs: the
    /// pill is the row's leftmost painted mark, and its own left edge is
    /// what the window border would clip, so this pins the exact inset
    /// where an ink-based bound could only assert `>= inset + PILL_PAD_X`.
    /// The panel's content edge is read back off the wash, which
    /// `draw_header` anchors to it at `HEADER_WASH_INSET`.
    ///
    /// And the inset is the *title column's*: the capsule's left edge must
    /// land on the boss name's, so the stat row reads as a third line of the
    /// same block rather than as a row indented by a number of its own.
    #[test]
    fn the_stat_row_ink_is_inset_from_the_panel_edge() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let panel_left = frame.gradient_box().left() - HEADER_WASH_INSET;
        let pill = frame.fill_box(TIMER_PILL_FILL);

        assert!(
            (pill.left() - (panel_left + HEADER_STAT_ROW_INSET_X)).abs() < 0.01,
            "the timer capsule starts at {} — {}pt inside the panel edge at \
             {panel_left}, not {HEADER_STAT_ROW_INSET_X}pt",
            pill.left(),
            pill.left() - panel_left
        );
        assert_eq!(
            HEADER_STAT_ROW_INSET_X,
            HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X,
            "the stat row's inset has drifted off the title column's indent"
        );
        let title = frame.text_box(&encounter_title(&snapshot.encounter));
        assert!(
            (pill.left() - title.left()).abs() < 0.01,
            "the timer capsule starts at {}, the boss name at {} — the stat row \
             is no longer aligned with the title column",
            pill.left(),
            title.left()
        );
        // The duration's own ink then clears the capsule's padding, so a
        // regression that kept the pill inset while un-padding the text
        // still fails here.
        let value = frame.text_box(&fmt_duration(snapshot.duration_ms));
        assert!(
            value.left() >= pill.left() + PILL_PAD_X - 0.01,
            "the duration starts at {} — inside the capsule's {PILL_PAD_X}pt \
             padding from {}",
            value.left(),
            pill.left()
        );
    }

    /// The gutter emblem and the stat row are separated *horizontally*, and
    /// nothing cuts the mark's bottom off to do it.
    ///
    /// Issue #75 clipped the horned mark to the header's *text* rows to keep
    /// its lower blade out of the stat-pill row. The box reaches 14pt past
    /// that band, so the clip did not just stop the blade — it sliced the
    /// diamond's bottom corner off, plainly visible in the composited window.
    /// The row moved right instead (`HEADER_STAT_ROW_INSET_X`), so the mark
    /// may now run its full box down into the stat row's vertical span while
    /// still touching neither the timer's capsule nor its digits.
    ///
    /// Both halves are asserted on one real frame, because either alone is
    /// cheatable: that the painted ink reaches the box's own bottom (the
    /// guard against the clip creeping back up to the text band), and that
    /// it lands on nothing — with the clearance that buys pinned exactly.
    ///
    /// A paint-level check, not a comparison of constants: the box geometry
    /// and the clip are two independent decisions, and only what the frame
    /// actually painted proves both are still right. The panel's own edges
    /// are read back off the wash gradient, which
    /// `the_wash_gradient_spans_the_whole_header_band` independently pins.
    #[test]
    fn the_gutter_emblem_clears_the_stat_row_horizontally_not_by_clipping() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let pill = frame.fill_box(TIMER_PILL_FILL);
        let stat_ink = frame.glyph_boxes(GlyphIcon::Timer)[0]
            .union(frame.text_box(&fmt_duration(snapshot.duration_ms)));
        let wash = frame.gradient_box();
        let panel_min = wash.min - egui::Vec2::splat(HEADER_WASH_INSET);
        let text_band_bottom = panel_min.y + header_text_band_height();
        // Where the box wants to end — 14pt below the text band, inside the
        // stat row's span, and the depth the paint has to actually reach.
        let box_bottom = header_emblem_rect(
            egui::Rect::from_min_size(panel_min, egui::vec2(wash.width(), TITLE_LINE_HEIGHT)),
            header_text_band_height(),
        )
        .bottom();
        assert!(
            box_bottom > text_band_bottom,
            "the emblem box no longer overflows the text band, so the \
             uncut-bottom assertion below proves nothing"
        );

        let emblems = frame.glyph_boxes(GlyphIcon::Emblem);
        assert_eq!(
            emblems.len(),
            2,
            "expected the gutter mark and the wash wallpaper: {emblems:?}"
        );
        // The gutter mark bleeds off the panel's left edge; the wash
        // wallpaper is right-aligned to the wash. Leftmost is the gutter.
        let gutter = emblems
            .iter()
            .copied()
            .min_by(|a, b| a.left().total_cmp(&b.left()))
            .expect("the header painted no emblem");

        // Uncut: the ink ends where the box does, not where the text band
        // does — the diamond keeps its bottom corner.
        assert!(
            (gutter.bottom() - box_bottom).abs() < 0.01,
            "gutter emblem ink stops at {}, short of its box's {box_bottom} \
             (the text band ends at {text_band_bottom}) — its bottom corner is \
             being clipped off again",
            gutter.bottom()
        );
        assert!(
            gutter.bottom() > pill.top(),
            "gutter emblem ink stops at {} — above the stat row starting at {}, \
             so this frame cannot show the two coexisting",
            gutter.bottom(),
            pill.top()
        );

        // …and lands on nothing, despite sharing those rows.
        assert!(
            !gutter.intersects(pill),
            "gutter emblem ink {gutter:?} lands on the timer pill {pill:?}"
        );
        assert!(
            !gutter.intersects(stat_ink),
            "gutter emblem ink {gutter:?} lands on the timer readout {stat_ink:?}"
        );
        // …because of where the row starts, which is the whole mechanism.
        let clearance = pill.left() - gutter.right();
        assert!(
            (clearance - HEADER_TEXT_PAD_X).abs() < 0.01,
            "the stat row starts {clearance}pt right of the emblem, not the \
             {HEADER_TEXT_PAD_X}pt the title column's indent buys it"
        );
        // And it is still a *gutter* decoration horizontally.
        assert!(
            gutter.right() <= panel_min.x + HEADER_GUTTER_WIDTH + 0.01,
            "gutter emblem ink runs to x={}, past the {HEADER_GUTTER_WIDTH}pt gutter",
            gutter.right()
        );
    }

    // -- title separator (fading slate-blue divider under the title) ------

    /// Pure-function version of the fade math (same reasoning as
    /// `share_bar_paints`): the leftmost segment must start at the accent
    /// stroke's peak alpha and fall off monotonically to zero at the right
    /// edge, never rising back up. Issue #56 lowered that peak from opaque
    /// to `TITLE_SEPARATOR_MAX_ALPHA` — the reference's stroke is a hairline
    /// accent, not a rule.
    #[test]
    fn title_separator_segments_fade_monotonically_from_full_to_zero() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 20.0));
        let segments = title_separator_segments(rect);

        assert!(!segments.is_empty());
        let first_alpha = segments.first().unwrap().1.a();
        let last_alpha = segments.last().unwrap().1.a();
        assert_eq!(
            first_alpha, TITLE_SEPARATOR_MAX_ALPHA,
            "leftmost segment should start at the stroke's peak alpha"
        );
        assert_eq!(
            last_alpha, 0,
            "rightmost segment should have faded to nothing"
        );

        let mut previous = 255;
        for (_, color) in &segments {
            assert!(color.a() <= previous, "alpha rose instead of fading");
            previous = color.a();
        }
    }

    /// The fade must not extend past the title row's own width — the whole
    /// point is that it fades out by roughly mid-width, not that it runs the
    /// full row.
    #[test]
    fn title_separator_segments_stay_within_rect_width() {
        let rect = egui::Rect::from_min_size(egui::pos2(5.0, 0.0), egui::vec2(200.0, 20.0));
        let segments = title_separator_segments(rect);
        for (seg_rect, _) in &segments {
            assert!(seg_rect.left() >= rect.left());
            assert!(seg_rect.right() <= rect.right() + 1.0);
        }
    }

    /// The faux-bold second pass is exactly what makes that degraded path
    /// still read as bold, so it must be on whenever the real bold is off.
    #[test]
    fn a_bold_paint_is_doubled_only_without_a_real_bold_font() {
        let ctx = egui::Context::default();
        let mut shapes = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint_bold_text(
                ui.painter(),
                egui::pos2(10.0, 10.0),
                egui::Align2::LEFT_CENTER,
                "Bahaar",
                FONT_SIZE_TITLE,
                TITLE_TEXT_COLOR,
            );
        });
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut shapes);
        }
        output.drop_without_applying_deltas();

        let expected = if fonts::has_real_bold() { 1 } else { 2 };
        assert_eq!(
            shapes.iter().filter(|text| *text == "Bahaar").count(),
            expected
        );
    }

    /// Issue #91: the timer used to sit flush against the panel's left
    /// border as a half-pill. It is now inset by `HEADER_STAT_ROW_INSET_X`
    /// along with the rest of the stat row, and that inset must be a real
    /// gap — not zero, and not so wide it eats the row's width budget
    /// (`the_stat_pills_fit_the_default_window_width` is the other half of
    /// that bargain).
    ///
    /// The value is not free to drift: it is the title column's own indent,
    /// so the timer sits under the boss name rather than at a margin of its
    /// own — and, since the gutter emblem's box ends at exactly
    /// `HEADER_GUTTER_WIDTH` while hanging into this row's height, that same
    /// sum is what holds the row clear of the mark (see
    /// `the_gutter_emblem_clears_the_stat_row_horizontally_not_by_clipping`).
    #[test]
    fn the_stat_row_is_inset_from_the_left_border() {
        const { assert!(HEADER_STAT_ROW_INSET_X > 0.0) };
        // The same sum `header_text_rect` indents the title and area name by.
        assert_eq!(
            HEADER_STAT_ROW_INSET_X,
            HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X
        );
        assert_eq!(HEADER_STAT_ROW_INSET_X, 36.0);
    }

    /// The stat row is one non-wrapping `ui.horizontal`: if the timer/DPS/
    /// damage pills and the status-toggle cluster ever stop fitting the
    /// default window width, the cluster gets pushed off the right edge
    /// rather than wrapping. Measured with real font metrics (the pills
    /// size themselves from their laid-out text) against realistic
    /// worst-case values. The window-control buttons this test used to
    /// budget for moved into the header dropdown (issue #71) and no longer
    /// occupy this row.
    #[test]
    fn the_stat_pills_fit_the_default_window_width() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        // Lay a frame out first so the real (non-empty) fonts are loaded.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        let row_height = BUTTON_ROW_HEIGHT;
        let measure = |value: &str, size: f32| {
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(value.to_owned(), bold(size), PILL_VALUE_COLOR)
                    .rect
                    .size()
            })
        };
        // A long fight at raid-boss numbers: the widest each pill gets.
        let timer = pill_size(
            measure("120:00", FONT_SIZE_TIMER),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        let dps = pill_size(
            measure("99.99M/s", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        let dmg = pill_size(
            measure("99.99B", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        // The toggle cluster (decision 5, issue #82; four buttons as of
        // issue #167): a fixed-width pill, not measured text.
        let toggles = 2.0 * TOGGLE_PAD_X
            + TOGGLE_MOUSE_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLOUD_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLICK_THROUGH_SIDE
            + TOGGLE_GAP
            + TOGGLE_ALWAYS_ON_TOP_SIDE;

        // Three gaps between the outer horizontal's four direct children
        // (timer, dps, dmg, toggle cluster).
        let gaps = 3.0 * 6.0;

        // Issue #91: the row no longer starts flush against the panel's
        // left content edge, so its inset is part of the width it needs.
        let total = HEADER_STAT_ROW_INSET_X + timer + dps + dmg + toggles + gaps;
        assert!(
            total <= default_inner_width(),
            "stat row needs {total}pt but the default window is only {}pt wide",
            default_inner_width()
        );
    }

    /// Share, Reset (issue #82) and History (issue #186) all paint at
    /// `TOGGLE_ACTIVE_COLOR`: the cluster holds three one-shot actions and
    /// no on/off state since issue #185 took click-through and
    /// always-on-top to the title row. History's tint is
    /// `toggle_state_tint(has_history)`, so an *available* history is what
    /// the active tint means here — see the disabled case below.
    #[test]
    fn the_toggle_cluster_renders_every_button_at_its_state_tint() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let share = icons.glyphs.get(GlyphIcon::Share).unwrap().id();
        let reset = icons.toolbar.get(ToolbarIcon::Reset).unwrap().id();
        let history = icons.glyphs.get(GlyphIcon::History).unwrap().id();

        let tints = |has_history: bool| -> Vec<(egui::TextureId, egui::Color32)> {
            let mut blits = Vec::new();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                toggle_cluster(
                    ui,
                    &tx_command,
                    &icons,
                    false,
                    true,
                    has_history,
                    &mut false,
                );
            });
            for clipped in &output.shapes {
                collect_image_texture_tints(&clipped.shape, &mut blits);
            }
            output.drop_without_applying_deltas();
            blits
        };

        let blits = tints(true);
        let tint_of = |id: egui::TextureId| blits.iter().find(|(t, _)| *t == id).map(|(_, c)| *c);
        for expected in [share, reset, history] {
            assert_eq!(
                tint_of(expected),
                Some(TOGGLE_ACTIVE_COLOR),
                "{expected:?} was not blitted at TOGGLE_ACTIVE_COLOR: {blits:?}"
            );
        }

        // Issue #186: the disabled state the old `draw_header_menu` item
        // carried survives the move as the dim "off" tint.
        let disabled = tints(false);
        assert_eq!(
            disabled
                .iter()
                .find(|(t, _)| *t == history)
                .map(|(_, c)| *c),
            Some(TOGGLE_OFF_COLOR),
            "History must blit at TOGGLE_OFF_COLOR with no history thread: {disabled:?}"
        );
    }

    /// All three toggle-cluster controls (Share, Reset — issue #82 — and
    /// History — issue #186) are real buttons: the tree must expose exactly
    /// three `Button` accesskit nodes, with no leftover inert decoration and
    /// none of the two controls issue #185 moved out to the title row.
    #[test]
    fn the_toggle_cluster_exposes_three_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        output.drop_without_applying_deltas();

        let button_count = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Button)
            .count();
        assert_eq!(
            button_count, 3,
            "expected Share, Reset and History to each expose a Button role, got {button_count}"
        );
    }

    /// PR #197 review: with no history thread the History button must be
    /// *genuinely* disabled, not merely dim and click-gated — accesskit has
    /// to publish `enabled: false`, or a screen-reader user hears a usable
    /// "History: unavailable" button, activates it, and nothing happens.
    #[test]
    fn the_history_button_reports_itself_disabled_without_history() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let disabled = |has_history: bool, label: &str| -> bool {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                toggle_cluster(
                    ui,
                    &tx_command,
                    &icons,
                    false,
                    true,
                    has_history,
                    &mut false,
                );
            });
            let update = output
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            output.drop_without_applying_deltas();
            // A direct `==` for the same target-dependent reason
            // `accessible_rect_for_label` spells out.
            update
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|s| s == label))
                .map(|(_, node)| node.is_disabled())
                .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"))
        };

        assert!(
            disabled(false, "History: unavailable"),
            "History must report enabled: false with no history thread"
        );
        assert!(
            !disabled(true, "History"),
            "History must stay enabled when there is a history thread"
        );
    }

    /// Mirrors `the_history_button_reports_itself_disabled_without_history`
    /// for Share (PR #225 review of issue #219): every existing
    /// `toggle_cluster` test call site passes `share_active = true`, so
    /// nothing verified the button is genuinely disabled — not merely
    /// dim — when it's false. Same contract as History: accesskit must
    /// publish `enabled: false`, and a click landing on the button's rect
    /// must not fire a capture.
    #[test]
    fn the_share_button_reports_itself_disabled_when_inactive() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let disabled = |share_active: bool, label: &str| -> bool {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                toggle_cluster(
                    ui,
                    &tx_command,
                    &icons,
                    false,
                    share_active,
                    true,
                    &mut false,
                );
            });
            let update = output
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            output.drop_without_applying_deltas();
            update
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|s| s == label))
                .map(|(_, node)| node.is_disabled())
                .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"))
        };

        assert!(
            disabled(false, "Copy screenshot to clipboard: unavailable"),
            "Share must report enabled: false when share_active is false"
        );
        assert!(
            !disabled(true, "Copy screenshot to clipboard"),
            "Share must stay enabled when share_active is true"
        );

        let label = "Copy screenshot to clipboard: unavailable";
        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, false, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, label).center();
        layout.drop_without_applying_deltas();

        let mut screenshot_requested = true;
        let output = ctx.run_ui(click_at(pos), |ui| {
            screenshot_requested =
                toggle_cluster(ui, &tx_command, &icons, false, false, true, &mut false);
        });
        output.drop_without_applying_deltas();
        assert!(
            !screenshot_requested,
            "clicking a disabled Share button must not fire a capture"
        );
    }

    /// Issue #185: the title row's own pill holds click-through and
    /// always-on-top, each at `toggle_state_tint` of its default `Settings`
    /// value — off (`TOGGLE_OFF_COLOR`) for click-through, on
    /// (`TOGGLE_ACTIVE_COLOR`) for always-on-top — and exposes both as real
    /// buttons.
    #[test]
    fn the_title_row_pill_renders_both_toggles_at_their_state_tint() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let click_through = icons.glyphs.get(GlyphIcon::MouseOff).unwrap().id();
        let always_on_top = icons.glyphs.get(GlyphIcon::Pin).unwrap().id();

        let mut blits = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        for clipped in &output.shapes {
            collect_image_texture_tints(&clipped.shape, &mut blits);
        }
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        output.drop_without_applying_deltas();

        let tint_of = |id: egui::TextureId| blits.iter().find(|(t, _)| *t == id).map(|(_, c)| *c);
        assert_eq!(
            tint_of(click_through),
            Some(TOGGLE_OFF_COLOR),
            "click-through defaults to off, so it must blit at TOGGLE_OFF_COLOR: {blits:?}"
        );
        assert_eq!(
            tint_of(always_on_top),
            Some(TOGGLE_ACTIVE_COLOR),
            "always-on-top defaults to on, so it must blit at TOGGLE_ACTIVE_COLOR: {blits:?}"
        );
        let button_count = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Button)
            .count();
        assert_eq!(
            button_count, 2,
            "expected click-through and always-on-top to each expose a Button role, got {button_count}"
        );
    }

    /// Issue #292: `toggle_state_tint` alone is a ~25% alpha delta on the
    /// same white glyph (`TOGGLE_OFF_COLOR` vs `TOGGLE_ACTIVE_COLOR`), which
    /// reads as nearly identical at a glance — not the "obvious at a
    /// glance" on/off signal click-through needs, since it flips a
    /// system-wide mouse-passthrough mode. This drives `CLICK_THROUGH_ON_FILL`,
    /// a second, high-contrast signal painted only while click-through is
    /// enabled, using the same `collect_circle_fills` helper the hover-fill
    /// suppression tests use.
    #[test]
    fn click_through_paints_a_distinct_fill_circle_only_while_enabled() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();

        let fills_for = |click_through: bool| -> Vec<egui::Color32> {
            let mut settings = Settings {
                click_through,
                ..Settings::default()
            };
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let title_row = test_title_row(ui);
                title_row_toggles(
                    ui,
                    SettingsHandle {
                        settings: &mut settings,
                        tx_settings: &tx_settings,
                    },
                    &icons,
                    title_row,
                    false,
                );
            });
            let mut fills = Vec::new();
            for clipped in &output.shapes {
                collect_circle_fills(&clipped.shape, &mut fills);
            }
            output.drop_without_applying_deltas();
            fills
        };

        let on = fills_for(true);
        assert!(
            on.contains(&CLICK_THROUGH_ON_FILL),
            "click-through enabled must paint CLICK_THROUGH_ON_FILL: {on:?}"
        );

        let off = fills_for(false);
        assert!(
            !off.contains(&CLICK_THROUGH_ON_FILL),
            "click-through disabled must not paint CLICK_THROUGH_ON_FILL: {off:?}"
        );
    }

    /// Clicking the click-through button flips `Settings::click_through`,
    /// tells the platform layer (`platform::set_click_through` — issue
    /// #167 rehash; a no-op stub off-Windows, so not independently
    /// observable from this test), and persists the change (issue #167).
    #[test]
    fn clicking_click_through_toggles_and_persists_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        assert!(!settings.click_through);
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, "Click-through: off").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(pos), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        output.drop_without_applying_deltas();

        assert!(settings.click_through, "the click must flip the flag on");
        let persisted = rx_settings.try_recv().expect("the click must persist");
        assert!(persisted.click_through);
    }

    /// Clicking the always-on-top button flips `Settings::always_on_top`,
    /// sends `ViewportCommand::WindowLevel(Normal)` (it starts on, so a
    /// click turns it off), and persists the change (issue #167).
    #[test]
    fn clicking_always_on_top_toggles_and_persists_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        assert!(settings.always_on_top);
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, "Always on top: on").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(pos), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert!(!settings.always_on_top, "the click must flip the flag off");
        assert!(
            commands.iter().any(|cmd| matches!(
                cmd,
                egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal)
            )),
            "the click must request WindowLevel(Normal): {commands:?}"
        );
        let persisted = rx_settings.try_recv().expect("the click must persist");
        assert!(!persisted.always_on_top);
    }

    /// Clicking the Share button fires the screenshot capture request; the
    /// clipboard write itself happens later, off `Event::Screenshot` (see
    /// `handle_screenshot_events_routes_a_screenshot_event_to_the_writer`).
    #[test]
    fn clicking_share_sends_a_screenshot_viewport_command() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, "Copy screenshot to clipboard").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(share_pos), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert!(
            commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::Screenshot(_))),
            "Share must request a screenshot: {commands:?}"
        );
        assert!(
            rx_command.try_recv().is_err(),
            "Share must not also send a UiCommand"
        );
    }

    /// Issue #156: the pointer is necessarily still over the Share button
    /// on the frame right after the click that started a screenshot
    /// capture (`ViewportCommand::Screenshot` captures "the next frame
    /// after" the request, per its own doc comment), so `toggle_button`'s
    /// hover circle would otherwise paint straight into the captured
    /// image. `capturing: true` must suppress it — for every button in the
    /// cluster, not just Share, since one guard covers the whole row.
    ///
    /// The `capturing: false` half is a sanity check on the test itself:
    /// without it, a `toggle_button` that suppressed its hover fill
    /// unconditionally would pass the `true` half for the wrong reason.
    #[test]
    fn toggle_button_suppresses_its_hover_fill_while_a_screenshot_capture_is_in_flight() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, "Copy screenshot to clipboard").center();
        layout.drop_without_applying_deltas();

        let hover_input = || egui::RawInput {
            events: vec![egui::Event::PointerMoved(share_pos)],
            ..Default::default()
        };

        let fills_while_hovered = |capturing: bool| -> Vec<egui::Color32> {
            let output = ctx.run_ui(hover_input(), |ui| {
                toggle_cluster(ui, &tx_command, &icons, capturing, true, true, &mut false);
            });
            let mut fills = Vec::new();
            for clipped in &output.shapes {
                collect_circle_fills(&clipped.shape, &mut fills);
            }
            output.drop_without_applying_deltas();
            fills
        };

        let normal = fills_while_hovered(false);
        assert!(
            normal.contains(&TOGGLE_HOVER_FILL),
            "sanity check: hovering Share with no capture in flight must \
             still paint the hover fill: {normal:?}"
        );

        let while_capturing = fills_while_hovered(true);
        assert!(
            !while_capturing.contains(&TOGGLE_HOVER_FILL),
            "the hover fill must be suppressed while a Share screenshot \
             capture is in flight: {while_capturing:?}"
        );
    }

    /// `toggle_button`'s doc comment and `capturing`'s call sites both claim
    /// suppression covers the tooltip as well as the hover circle — but the
    /// hover-fill test above only ever looks at painted circles, so it
    /// cannot catch a regression in the separate `if capturing { response }
    /// else { response.on_hover_text(label) }` branch that actually gates
    /// the tooltip. This drives that branch directly: `response.
    /// on_hover_text(label)` paints the label text itself (`ui.add(Label::
    /// new(text))` inside `Tooltip`'s contents), so its presence or absence
    /// in the painted `Shape::Text`s is a direct behavioral signature of
    /// which branch ran.
    ///
    /// `tooltip_delay` is zeroed so the tooltip shows on the very same
    /// frame the pointer arrives, instead of needing a run of frames with a
    /// stationary pointer to clear egui's real (0.5s) delay — this test
    /// only cares which branch `toggle_button` took, not egui's own hover
    /// timing.
    ///
    /// The `capturing: false` half is the same kind of sanity check as the
    /// hover-fill test's: without it, a harness that just can't produce a
    /// tooltip at all (wrong style, wrong input shape, ...) would pass the
    /// `true` half for the wrong reason.
    #[test]
    fn toggle_button_suppresses_its_tooltip_while_a_screenshot_capture_is_in_flight() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        ctx.global_style_mut(|style| style.interaction.tooltip_delay = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        const SHARE_LABEL: &str = "Copy screenshot to clipboard";

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, SHARE_LABEL).center();
        layout.drop_without_applying_deltas();

        let hover_input = || egui::RawInput {
            events: vec![egui::Event::PointerMoved(share_pos)],
            ..Default::default()
        };

        let texts_while_hovered = |capturing: bool| -> Vec<String> {
            let output = ctx.run_ui(hover_input(), |ui| {
                toggle_cluster(ui, &tx_command, &icons, capturing, true, true, &mut false);
            });
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                collect_text_shapes(&clipped.shape, &mut texts);
            }
            output.drop_without_applying_deltas();
            texts
        };

        // Same reason the menu-chevron test (`accessible_rect_for_label`'s
        // caller) needs a settle frame: `Popup::show` runs a just-opened
        // Area through a `sizing_pass` with no prior measured size, so its
        // first frame doesn't paint the same way every later frame does.
        // This frame only arms the tooltip; nothing here is asserted on.
        texts_while_hovered(false);

        let normal = texts_while_hovered(false);
        assert!(
            normal.iter().any(|text| text == SHARE_LABEL),
            "sanity check: hovering Share with no capture in flight must \
             still show its tooltip: {normal:?}"
        );

        let while_capturing = texts_while_hovered(true);
        assert!(
            !while_capturing.iter().any(|text| text == SHARE_LABEL),
            "the tooltip must be suppressed while a Share screenshot \
             capture is in flight: {while_capturing:?}"
        );
    }

    /// `handle_screenshot_events` is the routing half of the Share round
    /// trip: it must find a synthesized `Event::Screenshot` in this frame's
    /// input and hand its image to the writer, without needing a live
    /// window or a real clipboard.
    #[test]
    fn handle_screenshot_events_routes_a_screenshot_event_to_the_writer() {
        let ctx = egui::Context::default();
        let image = std::sync::Arc::new(egui::ColorImage::filled(
            [2, 2],
            egui::Color32::from_rgb(1, 2, 3),
        ));
        let input = egui::RawInput {
            events: vec![egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                user_data: egui::UserData::default(),
                image: image.clone(),
            }],
            ..Default::default()
        };

        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
        let output = ctx.run_ui(input, |_ui| {
            handle_screenshot_events(&ctx, |img| written.push(img));
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            written.len(),
            1,
            "the screenshot event must reach the writer exactly once"
        );
        assert_eq!(written[0].size, [2, 2]);
    }

    /// A frame with no `Event::Screenshot` in it must never call the
    /// writer — otherwise every idle frame would overwrite the clipboard.
    #[test]
    fn handle_screenshot_events_does_nothing_without_a_screenshot_event() {
        let ctx = egui::Context::default();
        let mut called = false;
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            handle_screenshot_events(&ctx, |_img| called = true);
        });
        output.drop_without_applying_deltas();

        assert!(
            !called,
            "the writer must not run without a Screenshot event"
        );
    }

    // -- Share screenshot cropping (issue #96) -------------------------------

    /// Issue #96 (PR #98 review): `pending_screenshot_bound` must be pinned
    /// to the row count as of the frame the Share click fired on — reading
    /// `self.snapshot` fresh when the async `Event::Screenshot` reply lands
    /// would crop against a row count that no longer matches the pixels
    /// actually captured (a player joining or dropping mid-encounter is
    /// routine during a real round trip). Exercises `OverlayApp`'s own
    /// field directly, not just the pure crop helpers, so a future
    /// refactor that reintroduces a reply-time read would be caught here —
    /// no egui round trip needed.
    #[test]
    fn pending_screenshot_bound_survives_a_later_snapshot_row_count_change() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );

        // What `OverlayApp::ui` does at the end of the frame the Share
        // click fired on: stash *that* frame's row bound, computed from
        // *that* frame's row count (3 rows).
        app.snapshot = rows_test_snapshot(3);
        let request_time_bound =
            rows_content_bottom_y(50.0, app.snapshot.rows.len(), ROW_HEIGHT, 500.0);
        app.pending_screenshot_bound = Some(request_time_bound);

        // A player joins mid-encounter before the async reply lands — the
        // row count changes underneath the pending request.
        app.snapshot = rows_test_snapshot(8);

        assert_eq!(
            app.pending_screenshot_bound,
            Some(request_time_bound),
            "the pending bound must stay pinned to the request-time row \
             count, not drift with the live snapshot"
        );
    }

    /// Fewer/shorter populated rows than the scroll area's allocated height
    /// must trim the bound to right after the last row, not the scroll
    /// area's full height — no trailing empty background.
    #[test]
    fn rows_content_bottom_y_trims_trailing_empty_space() {
        let bottom = rows_content_bottom_y(100.0, 3, 30.0, 500.0);
        assert_eq!(bottom, 100.0 + 3.0 * 30.0);
    }

    /// More populated-row content than the scroll area shows (scrolled
    /// off) must not push the bound past what the scroll area actually
    /// rendered — that content was clipped out of the frame itself.
    #[test]
    fn rows_content_bottom_y_clamps_to_the_scroll_area_when_rows_overflow_it() {
        let bottom = rows_content_bottom_y(100.0, 50, 30.0, 200.0);
        assert_eq!(bottom, 100.0 + 200.0);
    }

    /// Zero populated rows must not add any height past the chrome.
    #[test]
    fn rows_content_bottom_y_is_just_the_chrome_with_no_rows() {
        assert_eq!(rows_content_bottom_y(100.0, 0, 30.0, 500.0), 100.0);
    }

    /// The common case: converts a points bound to pixels via
    /// `pixels_per_point` and stays within the image.
    #[test]
    fn screenshot_crop_height_px_scales_by_pixels_per_point() {
        assert_eq!(screenshot_crop_height_px(200.0, 2.0, 1000), 400);
    }

    /// A non-positive bound (e.g. no bound stashed yet, so the crop falls
    /// back to `0.0`) must crop to nothing rather than underflow.
    #[test]
    fn screenshot_crop_height_px_floors_a_non_positive_bound_to_zero() {
        assert_eq!(screenshot_crop_height_px(0.0, 1.0, 600), 0);
        assert_eq!(screenshot_crop_height_px(-5.0, 1.0, 600), 0);
    }

    /// PR #98 review (clippy `neg_cmp_op_on_partial_ord`): a NaN bound or
    /// scale must floor to zero too, matching the original `!(x > 0.0)`
    /// behavior for every input (NaN comparisons are always `false`, so the
    /// negated form treated NaN as "not positive" — the rewritten
    /// `partial_cmp`-based check must preserve exactly that, not just the
    /// non-NaN cases).
    #[test]
    fn screenshot_crop_height_px_floors_a_nan_bound_or_scale_to_zero() {
        assert_eq!(screenshot_crop_height_px(f32::NAN, 1.0, 600), 0);
        assert_eq!(screenshot_crop_height_px(200.0, f32::NAN, 600), 0);
        assert_eq!(screenshot_crop_height_px(f32::NAN, f32::NAN, 600), 0);
    }

    /// Issue #96 (PR #98 review): the crop must be driven by the row count
    /// as of the frame the screenshot was actually *requested* on, not
    /// whatever row count is current by the time the async reply is
    /// handled — the row count is live combat data (a player joining or
    /// dropping mid-encounter is routine) and can change during the round
    /// trip. This exercises the pure crop helpers directly with two
    /// different row counts standing in for "request time" and "reply
    /// time", proving they produce different — and the request-time one is
    /// the one that must be used.
    #[test]
    fn crop_uses_the_bound_captured_at_request_time_even_if_the_row_count_later_changes() {
        let rows_top = 50.0;
        let row_height = 20.0;
        let rows_area_height = 500.0;
        let pixels_per_point = 1.0;

        // The frame the Share click fired on: 3 populated rows.
        let request_time_bound = rows_content_bottom_y(rows_top, 3, row_height, rows_area_height);

        // By the time the async reply lands, a player joined mid-encounter
        // and the row count grew — recomputing from *that* would produce a
        // different bound than what was actually captured.
        let reply_time_bound = rows_content_bottom_y(rows_top, 8, row_height, rows_area_height);
        assert_ne!(
            request_time_bound, reply_time_bound,
            "the test setup must actually exercise a row-count change"
        );

        let image = crop_test_image(4, 200);
        let cropped = crop_screenshot_to_rows(&image, request_time_bound, pixels_per_point);

        let expected_height =
            screenshot_crop_height_px(request_time_bound, pixels_per_point, image.size[1]);
        assert_eq!(cropped.size, [4, expected_height]);
        assert_ne!(
            cropped.size[1],
            screenshot_crop_height_px(reply_time_bound, pixels_per_point, image.size[1]),
            "must not crop as if it were computed from the reply-time row count"
        );
    }

    /// The primary case: trims the image down to the rows' bottom edge,
    /// keeping every pixel above it and dropping every row below.
    #[test]
    fn crop_screenshot_to_rows_trims_below_the_bound() {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 6.0, 1.0);
        assert_eq!(cropped.size, [4, 6]);
        assert_eq!(cropped.pixels, image.pixels[..4 * 6]);
    }

    /// When the computed bound reaches or exceeds the image's actual
    /// height (content overflows the scroll area, or a stale bound), the
    /// full capture is kept unmodified rather than degenerately cropped.
    #[test]
    fn crop_screenshot_to_rows_keeps_the_full_image_when_the_bound_is_not_smaller() {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 1000.0, 1.0);
        assert_eq!(cropped.size, image.size);
        assert_eq!(cropped.pixels, image.pixels);
    }

    /// Issue #82: a zero (or negative) row bound — e.g. the "no bound
    /// pinned yet" fallback `unwrap_or(0.0)` at the `crop_screenshot_to_rows`
    /// call site — used to make `screenshot_crop_height_px` return `0`,
    /// which `crop_screenshot_to_rows` then turned into a genuinely
    /// zero-height `ColorImage`. arboard rejects a zero-height image with
    /// `Error::ConversionFailure`, which is exactly the clipboard write
    /// failure reported in issue #82: every single Share click failed. A
    /// full, uncropped screenshot is a far better failure mode than no
    /// screenshot at all, so a degenerate crop height must fall back to the
    /// full image instead of an empty one.
    #[test]
    fn crop_screenshot_to_rows_falls_back_to_the_full_image_when_the_bound_is_non_positive() {
        let image = crop_test_image(4, 10);

        let cropped_zero = crop_screenshot_to_rows(&image, 0.0, 1.0);
        assert_eq!(cropped_zero.size, image.size);
        assert_eq!(cropped_zero.pixels, image.pixels);

        let cropped_negative = crop_screenshot_to_rows(&image, -5.0, 1.0);
        assert_eq!(cropped_negative.size, image.size);
        assert_eq!(cropped_negative.pixels, image.pixels);
    }

    /// Same degenerate-crop failure mode as above (issue #82), but driven
    /// by a non-positive `pixels_per_point` instead of a non-positive
    /// bound — both inputs feed the same `screenshot_crop_height_px` zero
    /// fallback, so both must produce the full image rather than a
    /// zero-height one.
    #[test]
    fn crop_screenshot_to_rows_falls_back_to_the_full_image_when_pixels_per_point_is_non_positive()
    {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 6.0, 0.0);
        assert_eq!(cropped.size, image.size);
        assert_eq!(cropped.pixels, image.pixels);
    }

    // -- pending screenshot bound take/consume (issue #82) -------------------

    /// Issue #82 (async round-trip regression): a frame with no screenshot
    /// event must leave whatever bound is pending untouched — the previous
    /// code unconditionally `take()`d `pending_screenshot_bound` every
    /// single frame regardless of whether that frame actually handled a
    /// screenshot reply, so the bound stashed on the request frame was
    /// discarded on the very next idle frame, long before the async
    /// `ViewportCommand::Screenshot` reply could ever land. By the time the
    /// reply arrived, the bound was already `None`, and the crop fell back
    /// to `unwrap_or(0.0)` — a zero row bound that produced a degenerate,
    /// zero-height clipboard image on every single Share click.
    #[test]
    fn take_pending_screenshot_bound_leaves_the_pending_bound_untouched_when_no_event_landed() {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(Some(123.0), false);
        assert_eq!(
            new_pending,
            Some(123.0),
            "a frame without a screenshot event must not consume the pending bound"
        );
        // Nothing reads `bound_to_use` when no event landed, but it must
        // still be a well-defined, harmless value.
        let _ = bound_to_use;
    }

    /// The frame that actually handles a screenshot event must consume the
    /// pending bound — using it for the crop and resetting the field to
    /// `None` so a later, unrelated frame can never reuse a stale value.
    #[test]
    fn take_pending_screenshot_bound_consumes_the_bound_when_an_event_lands() {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(Some(123.0), true);
        assert_eq!(bound_to_use, 123.0);
        assert_eq!(
            new_pending, None,
            "a landed event must consume (reset) the pending bound"
        );
    }

    /// A screenshot event landing with no bound ever pinned (e.g. the round
    /// trip somehow bypassed the request-time stash) must fall back to
    /// `0.0`, matching the previous `unwrap_or(0.0)` behavior, rather than
    /// panicking.
    #[test]
    fn take_pending_screenshot_bound_falls_back_to_zero_when_an_event_lands_with_no_pending_bound()
    {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(None, true);
        assert_eq!(bound_to_use, 0.0);
        assert_eq!(new_pending, None);
    }

    // -- screenshot_capture_guard (issue #156) --------------------------

    /// The guard must switch on the instant a Share click fires a new
    /// request — regardless of what it held before — so `toggle_cluster`'s
    /// suppression is armed before the very next frame, the one
    /// `ViewportCommand::Screenshot` actually captures.
    #[test]
    fn screenshot_capture_guard_switches_on_the_frame_a_request_is_fired() {
        assert!(screenshot_capture_guard(false, true, false));
        // A request firing on the same frame an old capture's reply lands
        // must still leave the guard set for the new capture in flight,
        // not clear it because a reply also landed this frame.
        assert!(screenshot_capture_guard(true, true, true));
    }

    /// A frame with neither a new request nor a landed reply must leave
    /// the guard exactly as it found it — this is what keeps the
    /// suppression alive across however many idle frames the async round
    /// trip takes.
    #[test]
    fn screenshot_capture_guard_holds_steady_on_an_idle_frame() {
        assert!(!screenshot_capture_guard(false, false, false));
        assert!(screenshot_capture_guard(true, false, false));
    }

    /// The frame the `Event::Screenshot` reply lands on clears the guard —
    /// suppression is only needed until the capture actually happens, and
    /// by the time the reply arrives it already has.
    #[test]
    fn screenshot_capture_guard_clears_when_the_reply_lands() {
        assert!(!screenshot_capture_guard(true, false, true));
    }

    /// The property the issue calls out explicitly: `ViewportCommand::
    /// Screenshot` captures "the next frame after" the one that sends it,
    /// and the `Event::Screenshot` reply can land any number of frames
    /// after *that* — so a guard that only covered the request frame would
    /// be a silent no-op (the request frame is never the one in the
    /// screenshot). Driving `screenshot_capture_guard` across a simulated
    /// frame sequence — request, the captured frame right after it, one
    /// more idle frame while the reply is still in flight, then the reply
    /// landing — proves the guard stays set for every frame in between,
    /// not just the click.
    #[test]
    fn screenshot_capture_guard_stays_set_through_the_captured_frame_and_every_frame_until_the_reply_lands()
     {
        let mut capturing = false;

        // Frame 0: the Share click fires the request. This frame itself is
        // never captured (`ViewportCommand::Screenshot` captures the frame
        // *after* this one), but the guard must already be set entering
        // the next frame.
        capturing = screenshot_capture_guard(capturing, true, false);
        assert!(
            capturing,
            "the guard must be set the instant the request fires"
        );

        // Frame 1: this is the frame that actually gets captured. No new
        // request, no reply yet — the guard must still be set here, or the
        // suppression never covers the one frame that matters.
        capturing = screenshot_capture_guard(capturing, false, false);
        assert!(
            capturing,
            "the guard must still be set on the captured frame, one frame after the click"
        );

        // Frame 2: still waiting on the async `Event::Screenshot` reply.
        capturing = screenshot_capture_guard(capturing, false, false);
        assert!(
            capturing,
            "the guard must stay set for every frame the reply hasn't landed on yet"
        );

        // Frame 3: the reply finally lands — the guard clears.
        capturing = screenshot_capture_guard(capturing, false, true);
        assert!(!capturing, "the guard must clear once the reply has landed");
    }

    /// `advance_screenshot_capture_wait` must reset to `0` on the exact
    /// same two triggers that reset `screenshot_capturing` itself — a new
    /// request, or a landed reply — so the two pure functions can never
    /// fall out of step (a stale non-zero count surviving a request/reply
    /// would let `screenshot_capture_timed_out` fire early on the next
    /// capture).
    #[test]
    fn advance_screenshot_capture_wait_resets_on_request_or_reply_and_counts_otherwise() {
        assert_eq!(advance_screenshot_capture_wait(0, true, false), 0);
        assert_eq!(advance_screenshot_capture_wait(5, true, false), 0);
        assert_eq!(advance_screenshot_capture_wait(5, false, true), 0);
        assert_eq!(advance_screenshot_capture_wait(5, true, true), 0);
        assert_eq!(advance_screenshot_capture_wait(0, false, false), 1);
        assert_eq!(advance_screenshot_capture_wait(5, false, false), 6);
    }

    /// `screenshot_capture_timed_out` is a plain threshold check against
    /// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES` — false below it, true at and
    /// past it (never lands, just keeps latching the guard closed on every
    /// later idle frame too).
    #[test]
    fn screenshot_capture_timed_out_trips_at_the_threshold() {
        assert!(!screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES - 1
        ));
        assert!(screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES
        ));
        assert!(screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES + 1
        ));
    }

    /// The actual bug this fallback exists for: a reply that never lands
    /// at all (the queued screenshot silently dropped somewhere in
    /// egui-wgpu's `Painter::paint_and_update_textures` — see
    /// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES`'s doc comment). Driving the
    /// three pure functions together, frame by frame, exactly as
    /// `OverlayApp::ui` does — `event_landed` is always `false` here, on
    /// every single frame — must still clear the guard once the timeout is
    /// reached, or `screenshot_capturing` would latch `true` forever and
    /// permanently suppress the toggle cluster's hover fill and tooltip.
    #[test]
    fn screenshot_capture_guard_clears_via_the_timeout_fallback_when_the_reply_never_lands() {
        let mut capturing = false;
        let mut frames_waited = 0u32;

        // Frame 0: the Share click fires the request.
        capturing = screenshot_capture_guard(capturing, true, false);
        frames_waited = advance_screenshot_capture_wait(frames_waited, true, false);
        assert!(capturing);

        // Every frame after that: the reply never lands. The guard must
        // still be set right up until the timeout, and clear exactly once
        // it's reached — never later, and never by some other means.
        for frame in 0..SCREENSHOT_CAPTURE_TIMEOUT_FRAMES {
            let timed_out = screenshot_capture_timed_out(frames_waited);
            assert!(
                !timed_out,
                "must not time out before frame {SCREENSHOT_CAPTURE_TIMEOUT_FRAMES}: at frame {frame}"
            );
            assert!(
                capturing,
                "the guard must stay set while waiting for a reply that hasn't timed out yet: frame {frame}"
            );
            capturing = screenshot_capture_guard(capturing, false, timed_out);
            frames_waited = advance_screenshot_capture_wait(frames_waited, false, timed_out);
        }

        assert!(
            screenshot_capture_timed_out(frames_waited),
            "the wait must have reached the timeout by now"
        );
        let timed_out = screenshot_capture_timed_out(frames_waited);
        capturing = screenshot_capture_guard(capturing, false, timed_out);
        assert!(
            !capturing,
            "the guard must clear via the timeout fallback when the reply never lands"
        );
    }

    // -- toggle_cluster: click-through / always-on-top (issue #167) --------

    #[test]
    fn toggle_state_tint_is_active_color_when_on() {
        assert_eq!(toggle_state_tint(true), TOGGLE_ACTIVE_COLOR);
    }

    #[test]
    fn toggle_state_tint_is_off_color_when_off() {
        assert_eq!(toggle_state_tint(false), TOGGLE_OFF_COLOR);
    }

    /// The tray escape hatch: a "Turn off click-through" request must force
    /// `click_through` off, regardless of which state it was in — see
    /// `click_through_after_tray_request`'s doc comment.
    #[test]
    fn click_through_after_tray_request_forces_it_off() {
        assert!(!click_through_after_tray_request(false, true));
        assert!(!click_through_after_tray_request(true, true));
    }

    /// An idle frame (no tray request) must leave `click_through` exactly
    /// as it found it, in either state — this is what keeps the poll from
    /// fighting the toggle button's own clicks on every other frame.
    #[test]
    fn click_through_after_tray_request_holds_steady_with_no_request() {
        assert!(!click_through_after_tray_request(false, false));
        assert!(click_through_after_tray_request(true, false));
    }

    // -- handle_share_screenshot (the `OverlayApp::ui` sequencing itself) ---

    /// `handle_share_screenshot` is the actual fix site for issue #82's
    /// async round-trip regression (see `take_pending_screenshot_bound`'s
    /// doc comment): it is the code that runs inside `OverlayApp::ui`, not
    /// just the pure helpers it calls. Driving `OverlayApp::ui` itself is
    /// infeasible — `eframe::Frame` has only `pub(crate)` fields and no
    /// public constructor — but this sequencing never actually touches
    /// `_frame`, so it was pulled out into its own function that needs
    /// only `&egui::Context` and `&mut Option<f32>`, both of which a bare
    /// `egui::Context::default()` and a plain local variable stand in for
    /// here, mirroring `handle_screenshot_events_routes_a_screenshot_
    /// event_to_the_writer`'s use of a synthetic `Event::Screenshot` with
    /// no live window or clipboard.
    ///
    /// A frame with no screenshot event must leave the pending bound
    /// untouched and must never call `write` — otherwise every idle frame
    /// between the Share click and the async reply would either discard
    /// the stashed bound (issue #82) or overwrite the clipboard with
    /// nothing to write.
    #[test]
    fn handle_share_screenshot_leaves_the_bound_untouched_on_a_frame_with_no_event() {
        let ctx = egui::Context::default();
        let mut pending_screenshot_bound = Some(123.0);
        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();

        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            handle_share_screenshot(&ctx, &mut pending_screenshot_bound, |image| {
                written.push(image);
            });
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            pending_screenshot_bound,
            Some(123.0),
            "a frame without a screenshot event must not consume the pending bound"
        );
        assert!(
            written.is_empty(),
            "write must not run without a Screenshot event"
        );
    }

    /// The frame that actually carries the async `Event::Screenshot` reply
    /// must consume (reset to `None`) the pending bound and hand exactly
    /// one cropped image to `write` — proving the whole sequencing inside
    /// `OverlayApp::ui` (collect → decide the bound → crop → write), not
    /// just its pure helpers in isolation.
    #[test]
    fn handle_share_screenshot_consumes_the_bound_and_writes_on_the_frame_the_event_lands() {
        let ctx = egui::Context::default();
        let mut pending_screenshot_bound = Some(6.0);
        let image = std::sync::Arc::new(egui::ColorImage::filled(
            [4, 10],
            egui::Color32::from_rgb(1, 2, 3),
        ));
        let input = egui::RawInput {
            events: vec![egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                user_data: egui::UserData::default(),
                image: image.clone(),
            }],
            ..Default::default()
        };

        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
        let output = ctx.run_ui(input, |_ui| {
            handle_share_screenshot(&ctx, &mut pending_screenshot_bound, |image| {
                written.push(image);
            });
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            pending_screenshot_bound, None,
            "a landed event must consume (reset) the pending bound"
        );
        assert_eq!(
            written.len(),
            1,
            "the landed screenshot must be cropped and written exactly once"
        );
        // pending bound of 6.0 at pixels_per_point 1.0 (the context's
        // default) crops the 10-tall source image down to 6 rows.
        assert_eq!(written[0].size, [4, 6]);
    }

    // -- header text gutter (issue #59, #62) -------------------------------

    /// The gutter is a fixed width, not a proportion of the window — unlike
    /// the old fractional indent, a narrow and a wide row must produce
    /// exactly the same left edge.
    #[test]
    fn header_gutter_is_a_fixed_width_regardless_of_the_window() {
        for width in [MIN_INNER_SIZE.x, default_inner_width(), 1_200.0] {
            let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(width, 20.0));
            let rect = header_text_rect(row);
            assert_eq!(
                rect.left() - row.left(),
                HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X
            );
        }
    }

    /// The title/subtitle text rect starts at the fixed gutter width and
    /// stops short of the strip reserved for issue #54's chevron, at every
    /// width the window can be dragged to.
    #[test]
    fn header_text_rect_is_indented_and_clears_the_right_control() {
        for width in [MIN_INNER_SIZE.x, default_inner_width(), 1_200.0] {
            let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(width, 20.0));
            let rect = header_text_rect(row);
            assert_eq!(
                rect.left(),
                row.left() + HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X
            );
            assert_eq!(rect.right(), row.right() - HEADER_RIGHT_CONTROL_WIDTH);
            assert!(
                rect.width() > 0.0,
                "no room left for a title at {width}pt wide"
            );
            assert_eq!(rect.top(), row.top());
            assert_eq!(rect.bottom(), row.bottom());
        }
    }

    /// Issue #185: the title row reserves the toggle pill's width on top of
    /// the chevron's, so the boss name never runs under either control —
    /// while the subtitle row, which has no pill, still reserves the chevron
    /// strip alone.
    #[test]
    fn the_title_row_reserves_the_toggle_pill_as_well_as_the_chevron() {
        for width in [MIN_INNER_SIZE.x, default_inner_width(), 1_200.0] {
            let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(width, 20.0));
            assert_eq!(
                title_text_rect(row).right(),
                row.right() - TITLE_RIGHT_CONTROLS_WIDTH
            );
            assert_eq!(
                header_text_rect(row).right(),
                row.right() - HEADER_RIGHT_CONTROL_WIDTH
            );
            assert!(
                title_text_rect(row).right() < header_text_rect(row).right(),
                "the title must stop short of the subtitle at {width}pt wide"
            );
        }
    }

    /// Issue #185: the title row's toggle pill sits immediately left of the
    /// chevron's reserved strip, at its full width, exactly inside the strip
    /// `title_text_rect` keeps clear for it — and the click-through button's
    /// published hit box is the pill's *first* slot, so the box that
    /// `WM_NCHITTEST` carves out really is the button the user sees.
    #[test]
    fn the_title_toggle_pill_sits_left_of_the_chevron() {
        let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(400.0, 22.0));
        let pill = title_toggle_pill_rect(row, 18.0);

        assert_eq!(
            pill.right(),
            row.right() - HEADER_RIGHT_CONTROL_WIDTH - TITLE_TOGGLE_GAP_X,
            "the pill's right edge is the chevron strip's left edge, minus the gap"
        );
        assert!(
            pill.right() < chevron_rect(row).left(),
            "the pill must sit clear of the chevron's own box"
        );
        assert_eq!(pill.width(), TITLE_TOGGLE_PILL_WIDTH);
        assert_eq!(pill.height(), 18.0);
        assert_eq!(pill.center().y, row.center().y);
        assert!(
            pill.left() >= title_text_rect(row).right(),
            "the pill must not overlap the title text"
        );

        let button = click_through_button_slot(pill);
        assert_eq!(button.left(), pill.left() + TOGGLE_PAD_X);
        assert_eq!(button.width(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(button.center().y, pill.center().y);
    }

    /// Issue #183: a capture of the transparent overlay is flattened to
    /// fully opaque before the clipboard write — the premultiplied RGB is
    /// left exactly as it is, only the alpha byte changes — while an
    /// already-opaque capture comes back as the very same `Arc`, with no
    /// second pixel buffer allocated.
    #[test]
    fn flatten_screenshot_alpha_forces_opacity_only_when_it_has_to() {
        let translucent = std::sync::Arc::new(egui::ColorImage::new(
            [2, 1],
            vec![
                egui::Color32::from_rgba_premultiplied(10, 20, 30, 40),
                egui::Color32::from_rgba_premultiplied(1, 2, 3, 255),
            ],
        ));
        let flattened = flatten_screenshot_alpha(&translucent);
        assert!(
            flattened.pixels.iter().all(|pixel| pixel.a() == u8::MAX),
            "every pixel must be opaque: {:?}",
            flattened.pixels
        );
        assert_eq!(
            (
                flattened.pixels[0].r(),
                flattened.pixels[0].g(),
                flattened.pixels[0].b()
            ),
            (10, 20, 30),
            "the premultiplied colour channels must be left untouched"
        );

        let opaque = std::sync::Arc::new(egui::ColorImage::new(
            [1, 1],
            vec![egui::Color32::from_rgba_premultiplied(1, 2, 3, 255)],
        ));
        assert!(
            std::sync::Arc::ptr_eq(&opaque, &flatten_screenshot_alpha(&opaque)),
            "an opaque capture must not be copied"
        );
    }

    /// An absurdly narrow row must degrade to an empty text rect rather than
    /// an inverted one (which would paint the title backwards through its
    /// clip).
    #[test]
    fn header_text_rect_never_inverts() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(20.0, 20.0));
        let rect = header_text_rect(row);
        assert!(rect.right() >= rect.left());
        assert_eq!(rect.width(), 0.0);
    }

    /// The separator bleeds `TITLE_SEPARATOR_LEFT_BLEED` back into the
    /// gutter from the title's own left edge (the source's `Margin="-5 ..."`)
    /// and clears the chevron's reserved strip on the right.
    #[test]
    fn title_separator_sits_below_the_title_row_and_clears_the_chevron() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(380.0, 20.0));
        let rect = title_separator_rect(row);
        let segments = title_separator_segments(rect);
        assert_eq!(
            segments.first().unwrap().0.left(),
            row.left() + HEADER_GUTTER_WIDTH - TITLE_SEPARATOR_LEFT_BLEED
        );
        assert!(
            (segments.last().unwrap().0.right() - (row.right() - HEADER_RIGHT_CONTROL_WIDTH)).abs()
                < 0.01
        );
        assert_eq!(rect.top(), row.bottom());
        assert_eq!(rect.bottom(), row.bottom() + TITLE_SEPARATOR_THICKNESS);
    }

    /// Regression for the misread WPF margin (`TITLE_SEPARATOR_TOP_OFFSET`,
    /// now removed): laid out with the exact `FontId` `paint_bold_text` uses
    /// for the title, a string with descenders must never have its ink
    /// crossed by the separator stroke, and the stroke must stay inside the
    /// `ITEM_SPACING_Y` gap between the title and subtitle rows rather than
    /// drifting into the subtitle.
    #[test]
    fn the_title_separator_clears_the_title_and_subtitle_glyphs() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        let row =
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(380.0, TITLE_LINE_HEIGHT));
        let galley = ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                "Bahaargypj".to_owned(),
                bold(FONT_SIZE_TITLE),
                TITLE_TEXT_COLOR,
            )
        });
        let ink_bottom =
            row.top() + (row.height() - galley.rect.height()) / 2.0 + galley.rect.bottom();

        let rect = title_separator_rect(row);
        assert!(
            rect.top() >= ink_bottom,
            "separator top {} cuts through the title's ink bottom {ink_bottom}",
            rect.top()
        );
        assert!(
            rect.bottom() <= row.bottom() + ITEM_SPACING_Y,
            "separator bottom {} drifts past the title/subtitle gap",
            rect.bottom()
        );
    }

    // -- encounter title/subtitle (issue #9 slice 2) -----------------------

    #[test]
    fn title_shows_boss_name_when_known_boss() {
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Rathalos"),
            is_boss: true,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Rathalos");
    }

    #[test]
    fn title_blank_for_unnamed_non_boss_id() {
        let e = EncounterInfo {
            boss_monster_id: Some(999_999),
            boss_name: None,
            is_boss: false,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "");
    }

    #[test]
    fn title_blank_for_named_but_non_boss_id() {
        // issue #42: a trash/monster pull (non-boss `boss_uid` target) shows
        // no name at all, even when the monster happens to have one in the
        // community table — `Meter::snapshot` already nulls `boss_name` for
        // this case, but this guards `encounter_title` itself against ever
        // falling back to a name (or a raw id) for a known-non-boss target.
        let e = EncounterInfo {
            boss_monster_id: Some(10_900),
            boss_name: None,
            is_boss: false,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "");
    }

    #[test]
    fn title_shows_placeholder_when_nothing_known() {
        assert_eq!(encounter_title(&EncounterInfo::default()), "No target");
    }

    // -- live boss now wins over the remembered one (issue #131) -----------

    #[test]
    fn title_prefers_the_live_boss_over_a_remembered_scene_boss() {
        // Issue #131's precedence inversion: a genuine currently-engaged
        // boss (`is_boss: true`) now outranks `scene_boss_name`, the
        // opposite of issue #125's original "final boss only, always" rule.
        // This is what makes the raid case below correct — see the raid
        // test and `encounter_title`'s doc comment for why.
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Rathalos"),
            is_boss: true,
            scene_boss_name: Some("Blazing Mech 05"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Rathalos");
    }

    #[test]
    fn title_prefers_scene_boss_name_before_any_hit_lands() {
        // The case the function's doc comment leads with: the dungeon's
        // final boss is already known from a prior pull, but this
        // encounter hasn't hit anything yet — `boss_monster_id` is `None`.
        // The scene boss name must still win, via the unconditional `if
        // let` ahead of the `match e.boss_monster_id`, not "No target".
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: Some("Blazing Mech 05"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Blazing Mech 05");
    }

    #[test]
    fn title_falls_through_to_existing_rules_when_scene_boss_name_absent() {
        // No dungeon final boss has been learned yet (e.g. a scene's first
        // run this session, or a non-dungeon scene) — the pre-issue-#125
        // precedence still applies unchanged.
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Rathalos"),
            is_boss: true,
            scene_boss_name: None,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Rathalos");
    }

    // -- multi-boss scenes ask for a selection (issue #150) ----------------

    #[test]
    fn title_asks_for_a_boss_selection_in_a_multi_boss_scene_with_nothing_engaged() {
        // Issue #150: in a raid that offers three separately selectable
        // bosses, `scene_boss_name` is deliberately suppressed (naming one
        // is a guess that is wrong two times out of three). "No target" is
        // the wrong caption for that: there is a target, the party just
        // hasn't picked it yet.
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: None,
            multi_boss_scene: true,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Select a boss");
    }

    #[test]
    fn title_keeps_no_target_in_an_ordinary_scene_with_nothing_engaged() {
        // The regression guard: an ordinary dungeon (or town) with nothing
        // engaged and nothing remembered is unchanged by issue #150.
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: None,
            multi_boss_scene: false,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "No target");
    }

    #[test]
    fn subtitle_shows_scene_name_when_known() {
        let e = EncounterInfo {
            scene_id: Some(1001),
            scene_name: Some("Frozen Bahaar's Sanctum"),
            ..Default::default()
        };
        assert_eq!(
            encounter_subtitle(&e),
            Some("Frozen Bahaar's Sanctum".to_string())
        );
    }

    #[test]
    fn subtitle_shows_scene_id_when_name_unknown() {
        let e = EncounterInfo {
            scene_id: Some(4242),
            scene_name: None,
            ..Default::default()
        };
        assert_eq!(encounter_subtitle(&e), Some("Scene #4242".to_string()));
    }

    #[test]
    fn subtitle_omitted_when_scene_unknown() {
        assert_eq!(encounter_subtitle(&EncounterInfo::default()), None);
    }

    // -- header_band_height (drag band must cover the rendered header) ----

    /// Issue #340: before the first frame has painted there is nothing to
    /// measure, so the sizing math keeps using the constant budget.
    #[test]
    fn measured_header_band_height_falls_back_to_the_budget_before_the_first_frame() {
        assert_eq!(
            measured_header_band_height(None),
            header_band_height(BUTTON_ROW_HEIGHT)
        );
    }

    /// Once `draw_header` has painted, its *measured* rect is what every
    /// dependent sizes against — a header that came out taller or shorter
    /// than the budget (a restyled `interact_size`, a longer text row) moves
    /// the sizing with it instead of leaving it pinned to the constant.
    #[test]
    fn measured_header_band_height_uses_the_rect_the_header_actually_painted() {
        let painted = egui::Rect::from_min_size(egui::pos2(4.0, 7.0), egui::vec2(300.0, 81.0));
        assert_eq!(measured_header_band_height(Some(painted)), 81.0);
        let shorter = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 50.0));
        assert_eq!(measured_header_band_height(Some(shorter)), 50.0);
    }

    /// egui stacks title + subtitle + stat row, so the band pays two gaps,
    /// not one, and they are not the same size: title -> subtitle pays
    /// `ITEM_SPACING_Y`, subtitle -> stat row pays `HEADER_STAT_ROW_GAP`.
    /// All three rows are unconditional (issue #91), so this is the band's
    /// only case — 68pt at the real button-row height.
    #[test]
    fn header_band_height_covers_both_text_rows_both_gaps_and_the_button_row() {
        let button_row_height = 18.0;
        let expected = TITLE_LINE_HEIGHT
            + ITEM_SPACING_Y
            + SUBTITLE_LINE_HEIGHT
            + HEADER_STAT_ROW_GAP
            + button_row_height;
        assert_eq!(header_band_height(button_row_height), expected);
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
    }

    /// Issue #91 regression. The header must be a fixed-height band whether
    /// or not an area name is known: the app's idle "No target" state used
    /// to skip the subtitle row *and* the gap above it, collapsing the band
    /// from 68 to 50 and lifting the entire stat-pill row — timer, DPS,
    /// damage, toggles — 18pt (`SUBTITLE_LINE_HEIGHT + ITEM_SPACING_Y`) up
    /// the window the moment the boss/area data arrived or went away. A
    /// pixel scan of the live window measured the band bottom at y=51
    /// without an area name against y=69 with one.
    ///
    /// Asserted twice over: on the layout helpers `draw_header` lays the
    /// band out with, and on where the stat row's chrome actually lands
    /// when the same snapshot is rendered with and without a scene name.
    #[test]
    fn a_missing_area_name_does_not_collapse_the_header_or_lift_the_stat_row() {
        // The band, and the stat row's offset into it, are single numbers —
        // there is no longer a subtitle-present/absent pair to diverge.
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
        let stat_row_top = header_text_band_height() + HEADER_STAT_ROW_GAP;
        assert_eq!(stat_row_top, 46.0);

        // Painted truth: `header_test_snapshot` has no scene at all, so it
        // is exactly the idle state that used to collapse the band.
        let without = header_test_snapshot(30_100_000_000);
        assert!(encounter_subtitle(&without.encounter).is_none());
        let mut with = header_test_snapshot(30_100_000_000);
        with.encounter.scene_name = Some("Frozen Bahaar's Sanctum");
        assert!(encounter_subtitle(&with.encounter).is_some());

        // The stat-pill chrome is the row's own ink, and it must not move
        // by so much as a point between the two. `PILL_FILL` picks the
        // first *value* pill (the timer wears `TIMER_PILL_FILL`, a colour
        // of its own), which is the same shape in both renders.
        let pill_without = header_painted_boxes(&without).fill_box(PILL_FILL);
        let pill_with = header_painted_boxes(&with).fill_box(PILL_FILL);
        assert!(
            (pill_with.top() - pill_without.top()).abs() < 0.01,
            "the stat-pill row sits at y={} with an area name but y={} without \
             it — a {}pt jump",
            pill_with.top(),
            pill_without.top(),
            pill_with.top() - pill_without.top()
        );
        assert!(
            (pill_with.height() - pill_without.height()).abs() < 0.01,
            "the stat-pill row is {}pt tall with an area name and {}pt without",
            pill_with.height(),
            pill_without.height()
        );
    }

    /// Issue #91's header grid: `TITLE_LINE_HEIGHT + ITEM_SPACING_Y +
    /// SUBTITLE_LINE_HEIGHT == 40.0`, grown from the source's `Height="36"`
    /// (`20 + 2 + 14`) so the boss name and area name each get the vertical
    /// room the reference render gives them. Pinned as a sum, not three
    /// separate literals, so a future edit to any one constant can't drift
    /// without this test catching it.
    #[test]
    fn the_title_and_subtitle_lines_add_up_to_the_source_header_grid() {
        let total = TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT;
        assert_eq!(total, 40.0);
        // …and the text band is that grid, whole, always (issue #91).
        assert_eq!(header_text_band_height(), total);
    }

    /// The whole point of issue #91's `HEADER_STAT_ROW_GAP`: the stat row
    /// must clear the subtitle's descenders by more than the 2pt every other
    /// adjacent pair gets, and the band must budget that extra room — 68pt
    /// with a subtitle, against the 60 it used to be.
    #[test]
    fn the_stat_row_gap_is_wider_than_the_ordinary_row_spacing() {
        const { assert!(HEADER_STAT_ROW_GAP > ITEM_SPACING_Y) };
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
    }

    /// The emblem is bled off the title row's left edge and overhangs the
    /// text band it decorates in *both* directions. Only the left bleed and
    /// the top overhang are ever cut (by the panel's own edges); the bottom
    /// overhang is painted in full, since `draw_header` clips the mark to
    /// the whole header band and the stat row is inset clear of it. Pinning
    /// the bottom overhang here is what keeps that so: shrink the box back
    /// inside the text band and the mark stops being the thing the reference
    /// render shows.
    #[test]
    fn the_header_emblem_bleeds_off_the_left_edge_and_out_of_the_text_band() {
        let row = egui::Rect::from_min_size(egui::pos2(10.0, 40.0), egui::vec2(380.0, 20.0));
        let band = header_text_band_height();
        let rect = header_emblem_rect(row, band);

        assert!(rect.left() < row.left());
        // …and by exactly enough that the visible remainder is the gutter.
        assert_eq!(rect.right() - row.left(), HEADER_GUTTER_WIDTH);

        assert!(rect.top() < row.top());
        assert!(rect.bottom() > row.top() + band);
        assert_eq!(rect.width(), HEADER_EMBLEM_SIZE);
        assert_eq!(rect.height(), HEADER_EMBLEM_SIZE);
    }

    /// The source's centering, reproduced exactly for the case it was read
    /// off (`Height="36"` grid, `Margin="… -8"`): a top edge 8pt above the
    /// grid, i.e. the box hangs `HEADER_EMBLEM_BOTTOM_BLEED` further below
    /// the band than above it.
    ///
    /// That asymmetry is the design, not an oversight — issue #91 squared it
    /// up on the assumption it was a bug and had to be reverted, so the
    /// numbers are pinned here. Against issue #91's 40pt text band the
    /// formula centres 60pt on `40 + 8 = 48`: top 6pt above the band (cut by
    /// the panel's own top edge) and bottom at `row.top() + 54`, 14pt below
    /// it — painted in full, down inside the stat row's height, which the
    /// row's own inset keeps clear of. See `header_emblem_rect`.
    #[test]
    fn the_header_emblem_hangs_further_below_the_text_band_than_above_it() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(380.0, 20.0));
        // The source's own 36pt grid — the case the `-8` margin was read off.
        assert_eq!(header_emblem_rect(row, 36.0).top(), -8.0);

        let band = header_text_band_height();
        let rect = header_emblem_rect(row, band);
        assert_eq!(rect.top(), row.top() - 6.0);
        assert_eq!(rect.bottom(), row.top() + 54.0);

        let above = row.top() - rect.top();
        let below = rect.bottom() - (row.top() + band);
        assert!(
            (below - (above + HEADER_EMBLEM_BOTTOM_BLEED)).abs() < 0.01,
            "the negative bottom margin should hang the emblem \
             {HEADER_EMBLEM_BOTTOM_BLEED}pt further below the band than above it: \
             above {above}, below {below}"
        );
    }

    /// The relation the emblem box's horizontal geometry rests on, pinned as
    /// constants so neither can be changed alone: shrinking or growing the
    /// mark without moving its left bleed would drag the visible gutter —
    /// and with it the title/subtitle indent — off `HEADER_GUTTER_WIDTH`.
    #[test]
    fn the_emblem_box_preserves_the_gutter() {
        const { assert!(HEADER_EMBLEM_LEFT_BLEED + HEADER_EMBLEM_SIZE == HEADER_GUTTER_WIDTH) };
        assert_eq!(HEADER_EMBLEM_LEFT_BLEED, -26.0);
        assert_eq!(HEADER_EMBLEM_SIZE, 60.0);
        // And the mark is deliberately *not* the band: it is shorter than
        // the drag band it is clipped to — so the clip never cuts its
        // bottom — and taller than the text band it is centered on, which is
        // what makes it overhang those rows at all.
        assert!(HEADER_EMBLEM_SIZE < header_band_height(BUTTON_ROW_HEIGHT));
        assert!(HEADER_EMBLEM_SIZE > header_text_band_height());
    }

    /// The band the drag surface covers is the text rows plus
    /// `HEADER_STAT_ROW_GAP` plus the stat-pill row, so the text band is
    /// strictly shorter than it. Both numbers exist and neither may be
    /// substituted for the other: the drag surface, the wash and the gutter
    /// emblem's clip are the band's, only the title/subtitle rows and the
    /// emblem's *centering* are the text band's.
    #[test]
    fn the_text_band_is_the_drag_band_minus_the_stat_pill_row() {
        let text = header_text_band_height();
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        assert_eq!(band, text + HEADER_STAT_ROW_GAP + BUTTON_ROW_HEIGHT);
        assert!(text < band);
        assert_eq!(text, 40.0);
    }

    /// …but never past the panel's own rounded, stroked border: the image
    /// has square corners, so an un-inset backdrop would poke out of the
    /// overlay's bottom corners. Same guarantee, and same inset, as
    /// `header_wash_rect`'s at the top.
    #[test]
    fn row_backdrop_stays_inside_the_panels_rounded_border() {
        let panel = wash_test_panel();
        // A row area that (as the real one does) runs to the panel's own
        // bottom and side edges.
        let available = egui::Rect::from_min_max(egui::pos2(panel.left(), 120.0), panel.max);

        let backdrop = row_backdrop_rect(available, panel);

        assert!(
            backdrop.left() >= panel.left() + HEADER_WASH_INSET,
            "{} pokes past the panel's left border",
            backdrop.left()
        );
        assert!(
            backdrop.right() <= panel.right() - HEADER_WASH_INSET,
            "{} pokes past the panel's right border",
            backdrop.right()
        );
        assert!(
            backdrop.bottom() <= panel.bottom() - HEADER_WASH_INSET,
            "{} pokes past the panel's bottom border",
            backdrop.bottom()
        );
    }

    /// The header wash and the row backdrop are two independent images
    /// (#253: "a user may want one, the other, or both"), so they must not
    /// overlap — the header's artwork ending mid-row, or the rows' artwork
    /// riding up over the title, would read as a rendering bug either way.
    #[test]
    fn row_backdrop_starts_below_the_header_wash() {
        let panel = wash_test_panel();
        let wash = header_wash_rect(panel, WASH_TEST_HEIGHT);
        // What `OverlayApp::ui`'s layout cursor has left once the header
        // band and its separator are behind it.
        let available =
            egui::Rect::from_min_max(egui::pos2(panel.left(), wash.bottom()), panel.max);

        let backdrop = row_backdrop_rect(available, panel);

        assert!(
            backdrop.top() >= wash.bottom(),
            "the row backdrop ({}) overlaps the header wash ({})",
            backdrop.top(),
            wash.bottom()
        );
    }

    /// Issue #297: the row area (and so the row backdrop image
    /// `draw_row_backdrop` paints into it) must begin at *exactly* the y
    /// `first_player_row_top_offset` predicts — the same y the header wash's
    /// bottom edge is anchored to (see `draw_header`'s `wash_height`) — or a
    /// sliver of bare panel fill shows between the two images at the
    /// banner/body seam.
    ///
    /// This drives `draw_header` and a real `ui.separator()` through an
    /// actual `egui::Ui` (the same layout `OverlayApp::ui` uses) rather than
    /// trusting the pure functions to agree with egui's own bookkeeping:
    /// `Ui::cursor` already carries one pending `item_spacing` past the last
    /// widget it placed, so the separator's *own* trailing advance adds a
    /// second one that `first_player_row_top_offset` must also count.
    #[test]
    fn the_row_area_begins_exactly_where_first_player_row_top_offset_predicts() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(default_inner_width(), default_inner_height(None)),
            )),
            ..Default::default()
        };

        let mut panel_top = 0.0_f32;
        let mut rows_top = 0.0_f32;
        let output = ctx.run_ui(input, |ui| {
            panel_top = ui.available_rect_before_wrap().top();
            draw_header(
                ui,
                &ctx,
                &snapshot,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                &mut WindowGesture::default(),
                None,
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                &mut 0,
                false,
                &mut false,
                None,
                &mut false,
            );
            // Exactly what `OverlayApp::ui` does between `draw_header` and
            // `draw_row_backdrop`: one `ui.separator()`, then read the
            // cursor it left behind.
            ui.separator();
            rows_top = ui.available_rect_before_wrap().top();
        });
        output.drop_without_applying_deltas();

        let band_height = header_band_height(BUTTON_ROW_HEIGHT);
        let predicted = panel_top + first_player_row_top_offset(band_height);
        assert!(
            (rows_top - predicted).abs() < 0.05,
            "the rows actually start at {rows_top}, but first_player_row_top_offset \
             (and so the header wash's bottom edge) predicts {predicted} — a {}pt gap \
             would open at the banner/body seam",
            rows_top - predicted
        );
    }

    /// A collapsed window can hand the layout a degenerate strip; that must
    /// produce an empty rect the painter skips, not a negative-size one.
    #[test]
    fn row_backdrop_of_an_empty_row_area_is_empty() {
        let panel = wash_test_panel();
        let available = egui::Rect::from_min_max(panel.max, panel.max);

        let backdrop = row_backdrop_rect(available, panel);

        assert!(backdrop.width() <= 0.0 || backdrop.height() <= 0.0);
    }

    /// The wash is inset from the panel on both sides and its top so its
    /// square corners stay inside the panel's rounded border, and it runs
    /// down exactly the `height` it is given rather than to the panel's —
    /// it is decoration behind the header rows, not a full-height
    /// background.
    #[test]
    fn the_header_wash_is_inset_from_the_panel_and_keeps_its_given_height() {
        let panel = wash_test_panel();
        let wash = header_wash_rect(panel, WASH_TEST_HEIGHT);
        assert_eq!(wash.left() - panel.left(), HEADER_WASH_INSET);
        assert_eq!(panel.right() - wash.right(), HEADER_WASH_INSET);
        assert_eq!(wash.top() - panel.top(), HEADER_WASH_INSET);
        assert_eq!(wash.width(), panel.width() - 2.0 * HEADER_WASH_INSET);
        assert_eq!(wash.height(), WASH_TEST_HEIGHT);
        assert!(wash.bottom() < panel.bottom());
    }

    /// The wash emblem hangs off the wash's right edge by exactly the
    /// source's `-25` right margin, and is vertically centered on the wash —
    /// the mirror of the gutter emblem's left-edge bleed, and the reason the
    /// wash must be painted through its own clip rect.
    #[test]
    fn the_wash_emblem_bleeds_off_the_right_edge_by_the_named_overhang() {
        let wash = header_wash_rect(wash_test_panel(), WASH_TEST_HEIGHT);
        let emblem = header_wash_emblem_rect(wash);
        assert_eq!(emblem.right() - wash.right(), HEADER_WASH_EMBLEM_BLEED);
        assert!(emblem.left() > wash.left());
        assert_eq!(emblem.center().y, wash.center().y);
        assert_eq!(emblem.width(), HEADER_WASH_EMBLEM_SIZE);
        assert_eq!(emblem.height(), HEADER_WASH_EMBLEM_SIZE);
    }

    /// Issue #255's live-window pass shrank `HEADER_WASH_EMBLEM_BLEED` from
    /// the source's literal `25` to `17` so the wash emblem stopped painting
    /// through the title row's toggle pill. Nothing tested that: the bleed's
    /// only other test compares the emblem's overhang against the very
    /// constant `header_wash_emblem_rect` computed it from, so it holds for
    /// any value — `25` included.
    ///
    /// The overlap that mattered is not a box overlap and cannot be tested
    /// as one. The emblem is a 200pt square blitted over a header band a
    /// quarter that tall, so its *box* swallows the pill whole at any bleed
    /// (asserted below, so nobody replaces this with a `!intersects` check
    /// that can only ever fail). What the user sees is the emblem's ink,
    /// which is a diamond symmetric about the square's vertical axis — the
    /// line the top chevron's vertex and both diamond apexes sit on, and the
    /// strongest edge anywhere in the mark. That axis is what has to clear
    /// the pill, and shrinking the overhang is what slides it left.
    #[test]
    fn the_wash_emblem_axis_clears_the_title_row_toggle_pill() {
        let panel = wash_test_panel();
        let wash = header_wash_rect(panel, WASH_TEST_HEIGHT);
        let emblem = header_wash_emblem_rect(wash);
        // The title row spans the panel's own width, the same stand-in
        // `the_gutter_emblem_clears_the_stat_row_horizontally_not_by_clipping`
        // builds from a painted frame's wash.
        let row = egui::Rect::from_min_size(panel.min, egui::vec2(wash.width(), TITLE_LINE_HEIGHT));
        let pill = title_toggle_pill_rect(row, 18.0);

        assert!(
            emblem.intersects(pill),
            "the emblem box {emblem:?} no longer covers the pill {pill:?} —              if that is now true by geometry, this test is testing nothing"
        );
        assert!(
            emblem.center().x < pill.left(),
            "the emblem's axis sits at {}, inside the toggle pill's {:?} —              its strongest edge is painting through the glyphs again",
            emblem.center().x,
            pill.x_range()
        );

        // …and it is the nudge that buys that, not the layout: at the
        // source's literal `25` the axis lands inside the pill.
        const SOURCE_BLEED: f32 = 25.0;
        let unnudged = emblem.center().x + SOURCE_BLEED - HEADER_WASH_EMBLEM_BLEED;
        assert!(
            unnudged > pill.left() && unnudged < pill.right(),
            "at the source's {SOURCE_BLEED}pt overhang the emblem's axis would              sit at {unnudged}, already clear of the pill's {:?} — the bleed no              longer drives this, so tighten or drop the assertion above",
            pill.x_range()
        );
    }

    /// The wash emblem is drawn far larger than the band it decorates, so it
    /// overflows the wash vertically too — a change that made it fit would
    /// mean it had stopped reading as an oversized watermark.
    #[test]
    fn the_wash_emblem_is_taller_than_the_wash_it_sits_in() {
        let wash = header_wash_rect(wash_test_panel(), WASH_TEST_HEIGHT);
        let emblem = header_wash_emblem_rect(wash);
        assert!(emblem.height() > wash.height());
        assert!(emblem.top() < wash.top());
        assert!(emblem.bottom() > wash.bottom());
    }

    /// Issue #91 inverts issue #81's rule. The wash is the header band's
    /// *background*, so it must run behind the stat-pill row as well as the
    /// text rows. Issue #158 corrects where it must stop: not
    /// `header_band_height` itself (that left an 8pt gap of bare panel fill,
    /// with the separator's faint line inside it, between the wash and the
    /// first row) but `first_player_row_top_offset` — the band plus the
    /// `ui.separator()` and the layout gap before it, which is where the
    /// first player row genuinely starts. What it must still never do is
    /// bleed *past* that row, which is exactly where the old fixed `98.0`pt
    /// wash went wrong.
    #[test]
    fn wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row() {
        let panel = wash_test_panel();
        let button_row_height = 18.0;
        let text_band = header_text_band_height();
        let band = header_band_height(button_row_height);
        let wash = header_wash_rect(panel, first_player_row_top_offset(band) - HEADER_WASH_INSET);
        let stat_pill_row_top = panel.top() + text_band + HEADER_STAT_ROW_GAP;
        let stat_pill_row_bottom = stat_pill_row_top + button_row_height;
        let first_player_row_top = panel.top() + first_player_row_top_offset(band);

        assert!(
            wash.top() < stat_pill_row_top,
            "wash top {} starts below the stat-pill row at {stat_pill_row_top}",
            wash.top()
        );
        assert!(
            wash.bottom() >= stat_pill_row_bottom,
            "wash bottom {} stops short of the stat-pill row's bottom at \
             {stat_pill_row_bottom}",
            wash.bottom()
        );
        assert!(
            wash.bottom() <= first_player_row_top,
            "wash bottom {} bleeds past the first player row at \
             {first_player_row_top}",
            wash.bottom()
        );
        // Flush, not merely inside: the only slack is the inset the wash
        // is pushed down from the panel's top edge by.
        assert_eq!(wash.bottom(), first_player_row_top);
    }

    /// Issue #91: the wash's *gradient* — not just its clip — has to span
    /// the whole band, measured on what `draw_header` actually paints. A
    /// change that shrank the quad back to the text band while leaving the
    /// emblem alone would pass every pure-geometry test above and still be
    /// the bug.
    ///
    /// Issue #158: asserting the gradient's *height* against `band -
    /// HEADER_WASH_INSET` is exactly what let the 8pt-short bug hide —
    /// that assertion was true both before and after the fix, since it
    /// never checks where the first player row actually starts. Asserting
    /// the gradient's *bottom* against `first_player_row_top_offset`
    /// instead (derived from `gradient.top()`, so this doesn't also have
    /// to assume where the panel's own top edge landed) ties the two
    /// together so they cannot drift apart again.
    #[test]
    fn the_wash_gradient_spans_the_whole_header_band() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let gradient = frame.gradient_box();

        let band = header_band_height(BUTTON_ROW_HEIGHT);
        // `gradient.top()` is `panel.top() + HEADER_WASH_INSET` (see
        // `header_wash_rect`), so subtracting the inset back out recovers
        // the panel's own top edge without this test having to assume a
        // fixed value for it.
        let panel_top = gradient.top() - HEADER_WASH_INSET;
        let first_player_row_top = panel_top + first_player_row_top_offset(band);
        assert!(
            (gradient.bottom() - first_player_row_top).abs() < 0.01,
            "the wash gradient's bottom is {}, not the first player row's \
             top at {first_player_row_top} — a wash that stops at the bare \
             header band leaves a gap of bare panel fill above the rows",
            gradient.bottom()
        );

        // …and it really is behind the stat row's ink, not merely tall.
        let stat_ink = frame.glyph_boxes(GlyphIcon::Timer)[0]
            .union(frame.text_box(&fmt_duration(snapshot.duration_ms)));
        assert!(
            gradient.bottom() >= stat_ink.bottom(),
            "the wash gradient ends at {} — above the timer's ink {stat_ink:?}",
            gradient.bottom()
        );
    }

    /// Issue #252: the header gutter emblem and wash (both its gradient
    /// fill and its own oversized emblem copy) must fade with the app's
    /// opacity slider exactly like the rest of the window's chrome —
    /// unchanged at the slider's top end, fully gone at its bottom end.
    /// Same shape as `panel_opacity_endpoints_are_solid_and_gone` above.
    #[test]
    fn header_emblem_and_wash_fade_with_the_opacity_slider() {
        for (name, color) in [
            ("gutter emblem", HEADER_EMBLEM_COLOR),
            ("wash emblem", HEADER_WASH_EMBLEM_COLOR),
            (
                "wash gradient top stop",
                egui::Color32::from_rgba_unmultiplied(0x70, 0x80, 0x90, HEADER_WASH_TOP_ALPHA),
            ),
        ] {
            assert_eq!(
                color.gamma_multiply(Settings::OPACITY_MAX).a(),
                color.a(),
                "{name} at 100% must keep its baked-in alpha"
            );
            assert_eq!(
                color.gamma_multiply(Settings::OPACITY_MIN).a(),
                0,
                "{name} at 0% must paint nothing"
            );
        }
    }

    /// Issue #252: the gutter emblem's own baked-in alpha must be strictly
    /// translucent — a bare `Color32::from_rgb` (implicit `0xFF`) painted
    /// the mark fully opaque, which is the bug this issue fixes.
    #[test]
    fn header_emblem_color_is_not_fully_opaque() {
        assert!(
            HEADER_EMBLEM_COLOR.a() < 255,
            "HEADER_EMBLEM_COLOR must carry a real alpha, not implicit full opacity"
        );
    }

    #[test]
    fn default_inner_height_matches_title_plus_header_plus_separator_plus_rows_plus_gaps() {
        let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        // Decision 3: every gap above the row list is inside the band bar
        // one — stat row->separator, the layout's ordinary
        // `ITEM_SPACING_Y`. `draw_rows` zeroes `item_spacing.y` for its own
        // scope, so there is no gap before the first row or between rows.
        // The band is spelled out here (rather than reusing
        // `header_band_height`, which is what the function itself calls) so
        // this stays an independent statement of the sum.
        let band = TITLE_LINE_HEIGHT
            + ITEM_SPACING_Y
            + SUBTITLE_LINE_HEIGHT
            + HEADER_STAT_ROW_GAP
            + BUTTON_ROW_HEIGHT;
        // Issue #297: egui's vertical layout pays `ITEM_SPACING_Y` on *both*
        // sides of the separator (once landing it after the band, once
        // landing the first row after it) — see
        // `first_player_row_top_offset`'s doc comment — so this independent
        // sum needs both, not one.
        let expected = band + SEPARATOR_HEIGHT + rows + 2.0 * ITEM_SPACING_Y;
        assert_eq!(default_inner_height(None), expected);
        // Issue #91 grew this from `652.0` -> `658.0` (a 2pt taller title
        // line, and `HEADER_STAT_ROW_GAP` above the stat row in place of
        // `ITEM_SPACING_Y`) -> `676.0` here: the band now reserves the
        // subtitle's line and gap whether or not an area name is known, so
        // the default window has to budget them too or it opens 18pt short
        // of the 20 rows it promises. Issue #297 then grew it to `678.0`:
        // the missing second `ITEM_SPACING_Y` around the separator was a
        // real 2pt gap at the banner/body seam, not just a test bug.
        assert_eq!(default_inner_height(None), 678.0);
    }

    #[test]
    fn reset_to_defaults_inner_height_matches_header_plus_separator_plus_five_rows_plus_gap() {
        let rows = RESET_TO_DEFAULTS_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        // Issue #297: both `ITEM_SPACING_Y` gaps around the separator — see
        // `first_player_row_top_offset`'s doc comment.
        let expected =
            header_band_height(BUTTON_ROW_HEIGHT) + SEPARATOR_HEIGHT + rows + 2.0 * ITEM_SPACING_Y;
        assert_eq!(reset_to_defaults_inner_height(None), expected);
    }

    /// The chevron lives entirely in the strip `header_text_rect` reserves
    /// for it, at every width the window can be dragged to — so a long boss
    /// name is clipped short of it rather than painted through it.
    #[test]
    fn the_chevron_stays_inside_the_reserved_strip_and_clears_the_title_text() {
        for width in [MIN_INNER_SIZE.x, default_inner_width(), 1_200.0] {
            let row = egui::Rect::from_min_size(
                egui::pos2(11.0, 5.0),
                egui::vec2(width, TITLE_LINE_HEIGHT),
            );
            let chevron = chevron_rect(row);
            let text = header_text_rect(row);

            assert!(
                row.contains_rect(chevron),
                "{width}pt: chevron escapes the row"
            );
            assert!(
                chevron.left() >= text.right(),
                "{width}pt: chevron {chevron:?} overlaps the title text {text:?}"
            );
            assert!(chevron.right() <= row.right());
            assert_eq!(chevron.center().y, row.center().y);
        }
    }

    #[test]
    fn the_click_through_slot_sits_after_share_and_reset() {
        let cluster = toggle_cluster_rect();
        let slot = click_through_button_slot(cluster);
        // Same running cursor `toggle_cluster` walks: pad, Share, gap,
        // Reset, gap — then the glyph box itself.
        assert_eq!(slot.left(), cluster.left() + CLICK_THROUGH_SLOT_OFFSET_X);
        assert_eq!(slot.width(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(slot.height(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(slot.center().y, cluster.center().y);
    }

    #[test]
    fn the_hit_box_pads_the_glyph_at_scale_one() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        assert_eq!(
            click_through_hit_box_px(button, 1.0),
            (
                (20.0 - CLICK_THROUGH_HIT_PAD) as i32,
                (30.0 - CLICK_THROUGH_HIT_PAD) as i32,
                (34.0 + CLICK_THROUGH_HIT_PAD) as i32,
                (44.0 + CLICK_THROUGH_HIT_PAD) as i32,
            )
        );
    }

    #[test]
    fn the_hit_box_scales_to_physical_pixels() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        // 2x DPI: every padded bound doubles.
        assert_eq!(click_through_hit_box_px(button, 2.0), (36, 56, 72, 92));
        // A fractional scale rounds rather than truncating.
        assert_eq!(click_through_hit_box_px(button, 1.5), (27, 42, 54, 69));
    }

    /// A bad `pixels_per_point` must never collapse the hit box: every bound
    /// would round to `0`, and `platform::Rect::contains` can't be true for a
    /// zero-area rect, so `WM_NCHITTEST` would answer `Transparent` for every
    /// point — including the button that turns click-through back off.
    #[test]
    fn a_degenerate_scale_falls_back_to_identity() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        let identity = click_through_hit_box_px(button, 1.0);
        for bad in [0.0, -1.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let (left, top, right, bottom) = click_through_hit_box_px(button, bad);
            assert_eq!(
                (left, top, right, bottom),
                identity,
                "pixels_per_point {bad} should fall back to 1.0"
            );
            assert!(
                right > left,
                "hit box must have area (pixels_per_point {bad})"
            );
            assert!(
                bottom > top,
                "hit box must have area (pixels_per_point {bad})"
            );
        }
    }
}
