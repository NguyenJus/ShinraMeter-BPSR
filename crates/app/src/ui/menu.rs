//! Header dropdown menu: toolbar icons, chevron, and the menu body.

use super::*;

/// Fixed display size, in points, every toolbar icon (issue #41) is drawn
/// at — independent of the source PNGs' own resolution (48x48 in the
/// upstream ShinraMeter set), so a texture swap can never change a menu
/// item's or the chevron's footprint. Plus `apply_theme`'s
/// `button_padding.y` on both sides, this lands on
/// `egui::Style::default().spacing.interact_size.y` (18.0) — see this
/// module's own `toolbar_icon_button_height_matches_interact_size`.
pub(crate) const TOOLBAR_ICON_SIZE: f32 = 14.0;

/// Tint applied to every toolbar/stat icon — the source's footer buttons are
/// `Fill="White"` at content `Opacity=".5"`, i.e. white at half alpha, not a
/// slate-blue-gray recolor.
pub(crate) const TOOLBAR_ICON_TINT: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 128);

/// Builds an `egui::Image` for a loaded toolbar icon texture at the fixed
/// `TOOLBAR_ICON_SIZE`, overriding whatever size the source PNG itself
/// carries (`SizedTexture::from_handle` would use the PNG's native 48x48
/// instead), and multiplied by `TOOLBAR_ICON_TINT` so every icon reads at
/// the same half-white opacity regardless of its source color.
pub(crate) fn toolbar_icon_image(handle: &egui::TextureHandle) -> egui::Image<'static> {
    egui::Image::from_texture(egui::load::SizedTexture::new(
        handle.id(),
        egui::Vec2::splat(TOOLBAR_ICON_SIZE),
    ))
    .tint(TOOLBAR_ICON_TINT)
}

/// Builds a menu-item `Button` for the header dropdown (issue #71): an
/// image-and-text button using the toolbar icon's existing texture when
/// `texture` is `Some`, or a plain text button — the same `Option` fallback
/// shape the old `icon_button` used — when it is `None` (belt-and-braces:
/// `ToolbarIcons`' bytes are compile-time constants, never actually expected
/// to fail to decode, same reasoning as `ClassIcons::get`).
pub(crate) fn menu_item_button<'a>(
    texture: Option<&egui::TextureHandle>,
    label: &'a str,
) -> egui::Button<'a> {
    match texture {
        Some(handle) => egui::Button::image_and_text(toolbar_icon_image(handle), label),
        None => egui::Button::new(label),
    }
}

// -- header menu chevron (issue #54, #71) --------------------------------
//
// The reference render puts a thin chevron at the far right of the title row.
// It is painted, not vendored: it is two strokes, no chevron exists in the
// upstream ShinraMeter icon set (`icons.rs`'s `TOOLBAR_ICON_BYTES`).
// Originally this toggled collapse-to-header directly; it now opens a
// dropdown menu instead (issue #71), so it always points down — a menu
// affordance, not a collapse-state indicator.

/// Side of the chevron's square hit/paint box, matched to `TOOLBAR_ICON_SIZE`
/// so it reads as one of the window controls rather than as decoration.
pub(crate) const CHEVRON_SIZE: f32 = TOOLBAR_ICON_SIZE;

/// Painted width of the V. The source's `ComboBoxToggleButton` chevron is a
/// `Path Width="10"`; the hit box stays `CHEVRON_SIZE` so the target is still
/// comfortable.
pub(crate) const CHEVRON_PAINT_WIDTH: f32 = 10.0;

/// Painted height of the V — a wide, shallow chevron, not an arrowhead.
pub(crate) const CHEVRON_PAINT_HEIGHT: f32 = 5.0;

/// The source's `Fill="#cfff"`.
pub(crate) const CHEVRON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0xCC);

/// Stroke width of the chevron. Thin, matching the reference's hairline
/// strokes, and a touch heavier than a hairline so it survives at 14pt.
pub(crate) const CHEVRON_STROKE: f32 = 1.5;

/// The chevron's square control box inside the title row's reserved
/// right-hand strip (`HEADER_RIGHT_CONTROL_WIDTH`, which `header_text_rect`
/// already keeps the title's own paint out of), centered in that strip both
/// ways.
///
/// Degrades rather than inverting at an absurdly narrow window, exactly like
/// `header_text_rect`: the strip is clamped against the row's left edge, and
/// the box is then clamped against the strip, so a hopeless width yields a
/// small-or-empty box inside the row instead of a backwards one.
pub(crate) fn chevron_rect(title_row: egui::Rect) -> egui::Rect {
    let left = (title_row.right() - HEADER_RIGHT_CONTROL_WIDTH).max(title_row.left());
    let strip = egui::Rect::from_min_max(
        egui::pos2(left, title_row.top()),
        egui::pos2(title_row.right(), title_row.bottom()),
    );
    let side = CHEVRON_SIZE.min(strip.width()).min(strip.height());
    egui::Rect::from_center_size(strip.center(), egui::Vec2::splat(side))
}

/// The title row's toggle pill (issue #185): a `TITLE_TOGGLE_PILL_WIDTH`
/// stadium holding the click-through and always-on-top buttons, sitting
/// `TITLE_TOGGLE_GAP_X` left of `chevron_rect`'s reserved strip and
/// vertically centered on the row.
///
/// `height` is the caller's `interact_size.y` — the same height
/// `toggle_cluster` allocates for the stat row's oval, so the two pills are
/// the same size — clamped to the row so a short row can't make the pill
/// paint outside it.
///
/// Degrades rather than inverting at an absurdly narrow window, exactly like
/// `chevron_rect` and `header_text_rect`: both edges are clamped against the
/// row's left edge, so a hopeless width yields a small-or-empty pill inside
/// the row instead of a backwards one.
pub(crate) fn title_toggle_pill_rect(title_row: egui::Rect, height: f32) -> egui::Rect {
    let right =
        (title_row.right() - HEADER_RIGHT_CONTROL_WIDTH - TITLE_TOGGLE_GAP_X).max(title_row.left());
    let left = (right - TITLE_TOGGLE_PILL_WIDTH).max(title_row.left());
    egui::Rect::from_center_size(
        egui::pos2((left + right) / 2.0, title_row.center().y),
        egui::vec2(right - left, height.min(title_row.height())),
    )
}

/// The three points of the chevron's polyline inside `rect`: a V opening
/// downward when `pointing_down`, and mirrored to open upward when not.
///
/// Down means "there is more below — click to fold it away" (the expanded
/// state, which is what the reference render shows); up means "click to
/// unfold" (collapsed). Pure, so the mirroring is unit-testable without a
/// painter — same reasoning as `pill_content_layout`.
pub(crate) fn chevron_points(rect: egui::Rect, pointing_down: bool) -> [egui::Pos2; 3] {
    let half_width = CHEVRON_PAINT_WIDTH / 2.0;
    let half_height = CHEVRON_PAINT_HEIGHT / 2.0;
    let center = rect.center();
    let tip_dy = if pointing_down {
        half_height
    } else {
        -half_height
    };
    [
        egui::pos2(center.x - half_width, center.y - tip_dy),
        egui::pos2(center.x, center.y + tip_dy),
        egui::pos2(center.x + half_width, center.y - tip_dy),
    ]
}

/// Paints the header menu control (issue #54, #71) into `rect` and returns
/// its `Response` — the trigger `Popup::menu` opens the dropdown from.
///
/// Registered with `ui.interact` on an explicit rect rather than allocated,
/// because it lives *inside* the title row `draw_title_line` already
/// allocated — in the strip that row deliberately keeps clear. It is
/// registered after `draw_header`'s title-bar drag surface, so it wins the
/// hit test over it and clicking the chevron never starts a window drag.
///
/// Same hand-supplied accessible name and tooltip as `minimize_button` used
/// to need, and for the same reason: a raw `interact` `Response` carries no
/// `WidgetInfo` from anywhere. Always points down (`chevron_points(rect,
/// true)`) — a menu affordance, not a collapse-state indicator — so the
/// label names what a click does ("Menu"), not a state.
pub(crate) fn menu_chevron(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Response {
    let label = "Menu";
    let response = ui.interact(rect, ui.id().with("menu_chevron"), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        ui.painter().add(egui::Shape::line(
            chevron_points(rect, true).to_vec(),
            egui::Stroke::new(CHEVRON_STROKE, CHEVRON_COLOR),
        ));
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), label)
    });
    response.on_hover_text(label)
}

/// Issue #231: the total clearance kept between the header dropdown's
/// capped `ScrollArea` height and the full screen height — see
/// `header_menu_scroll_max_height`, which subtracts this once from
/// `screen_height`. Keeps the menu from ever claiming literally every
/// pixel of a display it happens to fit inside; since the popup already
/// opens below the header rather than at the very top of the screen, this
/// margin's practical effect is clearance at the bottom, the same margin
/// the popup already keeps clear of a screen edge horizontally.
pub(crate) const HEADER_MENU_SCROLL_MARGIN: f32 = 48.0;

/// Issue #231: caps how tall the header dropdown's `ScrollArea` (wrapped
/// around `draw_header_menu`'s body) may grow, so a long Columns list
/// scrolls instead of pushing the menu — and whatever's below the
/// overflow, like Close — past the bottom of the screen where nothing
/// could reach it.
///
/// `screen_height` is `egui::Context::viewport_rect().height()` — this
/// overlay is a single, undecorated viewport with no OS chrome around it,
/// so its viewport rect *is* the full on-screen area the popup has to fit
/// inside. Pure and separate from the `Ui`/`Context` that actually draws
/// the menu, the same way this module's other pixel-math helpers
/// (`pill_size`, `row_content_width`, …) stay unit-testable without a live
/// egui frame.
pub(crate) fn header_menu_scroll_max_height(screen_height: f32, margin: f32) -> f32 {
    (screen_height - margin).max(0.0)
}

/// The header dropdown (issue #54, #71): opened from `menu_chevron` via
/// `egui::Popup::menu`, replacing the old row of window-control icon
/// buttons. Built from plain `Ui` menu widgets rather than one bespoke
/// helper per item, since egui's own `menu_button`/`CollapsingHeader`
/// already give every item free open/close state management.
///
/// Issue #231: the whole body below is wrapped in a vertical `ScrollArea`
/// capped at `header_menu_scroll_max_height` — before this, an expanded
/// Columns section could make the menu taller than the screen, and with no
/// scrollbar of its own the items past the bottom edge were simply
/// unreachable (the popup itself isn't a native window, so there's no OS
/// chrome to scroll it either). The cap uses the full screen height rather
/// than the popup's actual on-screen headroom (distance from its top to
/// the screen bottom) because egui doesn't expose that until *after* the
/// popup has already been laid out once — using the coarser, always-
/// available screen height instead means the menu can scroll a little
/// sooner than strictly necessary near the very top of a display, which is
/// a harmless trade against the alternative of a one-frame-stale bound.
///
/// Order matches the spec: a Columns disclosure section (issue #13's stat
/// column toggles, unchanged in behavior — just relocated), a separator,
/// the Opacity slider (issue #166), a separator, Minimize to tray, a
/// separator, Reset to defaults (issue #203), Export logs (issue #220), and
/// Export session bundle (`crate::bundle`), a separator, Check for updates
/// and its result line (issue #171), a
/// separator, then Close. "Forget learned bosses" (issue #131) sat between
/// the Opacity slider and Minimize until issue #201 replaced the runtime
/// scene -> boss learning it reset with a curated static table
/// (`bpsr_meter::tables::SCENE_FINAL_BOSSES`), leaving nothing to forget.
/// Reset used to be the first item here but moved
/// into the header's toggle cluster (issue #82; see `toggle_cluster`),
/// leaving this menu with no reset trigger of its own.
/// Collapse/Expand (issue #54's collapse-to-header) used to be the item
/// between Columns and Minimize; it was removed outright rather than
/// reworked, since the chevron no longer indicates a collapse state to
/// toggle (see `menu_chevron`).
///
/// Columns renders its checkboxes inline behind a click-to-expand
/// `CollapsingHeader` rather than the hover `SubMenu` flyout it used to be
/// (issue #93). The flyout had two compounding problems in this egui
/// version: it opened purely on hover with no dwell delay, and its own
/// `CloseOnClickOutside` config (needed for the reason below) meant leaving
/// the hover didn't close it either — an accidental hover trapped the
/// pointer until an explicit outside click. It also requested
/// `RectAlign::RIGHT_START` but could fall back to the mirrored
/// `LEFT_START` near a screen edge, opening back over the trigger instead
/// of beside it — exactly what this screen-edge overlay routinely sits
/// near. A disclosure section has no second popup layer to mistime or
/// mis-position, so neither problem is reachable.
///
/// Built from `collapsing_header::CollapsingState::show_header` rather than
/// the plain `egui::CollapsingHeader::new(...).show(...)` shorthand so the
/// header can carry `ToolbarIcon::Settings` beside the "Columns" label —
/// otherwise this item would have no icon call site while every other item
/// in the menu (Close) still does, an inconsistency review flagged on this
/// PR.
///
/// Losing the submenu also loses the `close_behavior` it carried on its own:
/// the checkboxes are now direct children of the *root* popup, so
/// `draw_header`'s `close_behavior(CloseOnClickOutside)` on that root popup
/// does the checkbox-safe job the submenu's own config used to — a
/// checkbox click no longer dismisses the whole dropdown. Minimize/Close
/// call `ui.close()` themselves so they still dismiss it on click, matching
/// the root's old `CloseOnClick` default for those two items.
// Issue #39: same reasoning as `draw_header`'s identical allow just above —
// one more history-view parameter tips this over clippy's default limit.
#[allow(clippy::too_many_arguments)]
/// The status line under one background-image row in the settings dropdown
/// (issues #121, #253): the chosen file's name, or — when `error` says the
/// load failed — that name prefixed with a warning and followed by the
/// reason.
///
/// This is the "surface something to the user rather than failing silently"
/// half of the failure story. Without it a mistyped path in a hand-edited
/// settings.json, or artwork the user has since moved, simply looks like
/// the feature does not work: the overlay would keep painting its default
/// artwork with nothing anywhere saying why.
///
/// Shows the file name rather than the full path — the dropdown is a narrow
/// popover over a game, and a full Windows path wraps into several lines of
/// it — with the full path on the row's hover tooltip instead (see
/// `background_image_row`). Pure, and split out for the same reason
/// `title_separator_segments` is: unit-testable without a live `egui::Ui`.
pub(crate) fn background_image_status(path: &Path, error: Option<&ImageError>) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        // A path ending in `..` or a bare root has no file name; showing
        // the whole thing beats showing an empty label.
        .unwrap_or_else(|| path.display().to_string());
    match error {
        Some(err) => format!("⚠ {name} {err}"),
        None => name,
    }
}

/// One row of the settings dropdown's "Background images" section: the
/// region's name, a button that opens the native picker, a button that
/// clears it, and the status line below (issues #121, #253).
///
/// Both buttons clear this slot's cache entry before sending: the path is
/// the cache key, so a *different* path would invalidate on its own, but
/// re-picking the *same* path is precisely how a user says "I have replaced
/// that file, load it again", and clearing here is what makes that work.
///
/// That `clear` happens after this frame's status label is already drawn
/// below, though: a re-pick still leaves one frame where `settings` reports
/// the new path but the cache entry is the old one's. Rather than relying
/// on draw order to dodge that window, `CustomImages::error` itself refuses
/// to hand back a cached failure whose `Entry.path` doesn't match the path
/// being asked about — see its doc comment — so a stale error can never be
/// attributed to a different file no matter which order things repaint in.
pub(crate) fn background_image_row(
    ui: &mut egui::Ui,
    slot: ImageSlot,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
    icons: &Icons,
) {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(slot.label());
        if ui.button("Choose…").clicked() {
            // Inline on this thread, like "Export logs"' save dialog — see
            // `platform::choose_log_export_path`'s doc comment for why a modal
            // the OS is already blocking on needs no thread of its own. `None`
            // means the user cancelled, which must leave the current choice
            // alone rather than clearing it.
            if let Some(path) = crate::platform::choose_background_image_path(slot.label()) {
                settings.set_background_image(slot, Some(path));
                changed = true;
            }
        }
        let configured = settings.background_image(slot).is_some();
        if ui
            .add_enabled(configured, egui::Button::new("Clear"))
            .clicked()
        {
            settings.set_background_image(slot, None);
            changed = true;
        }
    });
    if let Some(path) = settings.background_image(slot) {
        let error = icons.custom.borrow().error(slot, path);
        // Inset from the right before the label wraps. Every other row in
        // this dropdown is a `ui.horizontal` strip of intrinsically sized
        // widgets, so nothing else here ever reaches the popup's right
        // content edge; the `⚠ …` status is the one label long enough to
        // wrap, and egui wraps it at the full content width — which the
        // `ScrollArea`'s *floating* scrollbar overlays rather than reserves
        // space for (`ScrollStyle::floating` allocates no width), so the
        // message ran hard into the panel edge and under the bar. Taking
        // the scrollbar's own width plus its inner margin back gives the
        // wrapped text the same breathing room the rest of the section has.
        let inset = {
            let scroll = &ui.spacing().scroll;
            scroll.bar_width + scroll.bar_inner_margin
        };
        let wrap_width = (ui.available_width() - inset).max(1.0);
        ui.scope(|ui| {
            ui.set_max_width(wrap_width);
            ui.label(background_image_status(path, error.as_ref()))
                .on_hover_text(path.display().to_string());
        });
    }
    if changed {
        icons.custom.borrow_mut().clear(slot);
        // Same persistence path as the Columns checkboxes and the opacity
        // slider: blocking file IO stays off this render thread.
        let _ = tx_settings.send(settings.clone());
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_header_menu(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    // Issue #171: the manual "Check for updates" item's in-flight/last-
    // result state — see `UpdateCheckState`'s doc comment.
    update_check: &mut UpdateCheckState,
    // Issue #220: the reply channel each "Export logs" or "Export session
    // bundle" click's spawned thread sends its outcome back over — see
    // `LogExportOutcome` and this module's own doc comment for why the two
    // items share one channel.
    tx_log_export: &Sender<LogExportOutcome>,
    // Issue #321: set when the Close item below actually sends
    // `UiCommand::Quit`, so `OverlayApp::ui` can flag `self.quit_requested`
    // — the callers of that flag need to know an orderly quit is under way
    // *before* the pipeline thread's snapshot channel disconnects, so
    // `drain_snapshots` can tell that disconnect apart from a dead
    // pipeline (issue #214's real failure mode) instead of logging a false
    // "the meter is frozen" error on every clean shutdown.
    quit_requested: &mut bool,
) {
    let SettingsHandle {
        settings,
        tx_settings,
    } = settings;

    let scroll_max_height =
        header_menu_scroll_max_height(ctx.viewport_rect().height(), HEADER_MENU_SCROLL_MARGIN);
    // `egui::Popup`'s `Area` remembers its content size across frames and
    // hands the *same* remembered height back to this `Ui` as its
    // `max_rect` next frame (that's how a popup "grows to fit content" at
    // all). A plain `ScrollArea::max_height` alone can't override that: its
    // own footprint is capped by whatever height this `Ui` was *given*, not
    // by the argument passed here — so once an early frame (Columns still
    // collapsed) commits a small remembered size, the `ScrollArea` always
    // reports back "I fit", and the `Area` never learns it could grow
    // further, even on a screen with plenty of room. `set_max_height`
    // overrides that stale, self-referential ceiling with the freshly
    // computed cap on every frame, breaking the feedback loop; the
    // `ScrollArea` below still shrinks to true content when that is
    // smaller, so a short/collapsed menu paints exactly as before.
    ui.set_max_height(scroll_max_height);
    egui::ScrollArea::vertical()
        .max_height(scroll_max_height)
        // Shrinks to the content's actual height whenever that's under the
        // cap, so a short menu (the common case — Columns collapsed) keeps
        // painting pixel-identically to before this issue, with no
        // reserved scrollbar gutter and no extra bottom padding.
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let columns_id = ui.make_persistent_id("header_menu_columns");
            let mut columns_state =
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ctx, columns_id, false,
                );
            // Built from `CollapsingState` directly, rather than the plain
            // `egui::CollapsingHeader::new(...).show(...)` shorthand this replaced,
            // so the header can carry `ToolbarIcon::Settings` beside the "Columns"
            // label. `show_toggle_button`'s own click target is only the small
            // disclosure-arrow box, unlike `CollapsingHeader::show`'s whole clickable
            // row — so the icon+label response is unioned and toggled by hand below,
            // to keep the same full-row click target rather than shrinking it down
            // to the arrow alone.
            let header_response = ui.horizontal(|ui| {
                columns_state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
                // A bare `ui.label` only senses hover, never clicks — `Sense::click()`
                // is what actually lets the union below toggle on a label/icon click
                // rather than doing nothing.
                let mut header_response =
                    ui.add(egui::Label::new("Columns").sense(egui::Sense::click()));
                if let Some(handle) = icons.toolbar.get(ToolbarIcon::Settings) {
                    // Same reasoning as the label above: `toolbar_icon_image` builds
                    // a plain `egui::Image`, hover-sensing only by default, so it
                    // needs an explicit click sense to actually contribute to the
                    // unioned header-row click target.
                    header_response |=
                        ui.add(toolbar_icon_image(handle).sense(egui::Sense::click()));
                }
                header_response
            });
            if header_response.inner.clicked() {
                columns_state.toggle(ui);
            }
            columns_state.show_body_indented(&header_response.response, ui, |ui| {
                let mut changed = false;
                for col in ColumnKind::ALL {
                    let is_visible = settings.is_visible(col);
                    // Disabling the last remaining column would leave the row
                    // with nothing to show, so its checkbox is greyed out and
                    // inert rather than letting the click land (issue #13's
                    // "keep the UI usable" guard).
                    let would_disable_last = is_visible && settings.visible_columns.len() <= 1;
                    let mut checked = is_visible;
                    let resp = ui.add_enabled(
                        !would_disable_last,
                        egui::Checkbox::new(&mut checked, col.label()),
                    );
                    if resp.changed() {
                        settings.toggle(col);
                        changed = true;
                    }
                }
                if changed {
                    // Persisting is blocking file IO (`fs::write` + `fs::rename`),
                    // so it must not run on this render thread — hand the new
                    // value to the dedicated settings-writer thread instead,
                    // same as `pipeline::spawn` owns the meter off this thread.
                    // A disconnected receiver (writer thread gone) is not
                    // fatal: the in-memory `settings` the UI already mutated
                    // stays correct for the rest of this session.
                    let _ = tx_settings.send(settings.clone());
                }
            });

            ui.separator();

            // Issue #166: its own labelled section (not nested inside the Columns
            // disclosure above) since it toggles a single overlay-wide value rather
            // than a per-column list — a `CollapsingHeader` would just be an extra
            // click for something that's already only one control. Placed in the
            // header's settings dropdown, alongside Columns, rather than the
            // header's toggle-cluster pill (a separate, unrelated header surface —
            // see `toggle_cluster` — that a different issue is changing in
            // parallel).
            ui.label("Opacity");
            // Issue #182: the rail spans the full 0%-100%, floor included. Nothing
            // here has to guard the bottom end — `Settings::OPACITY_MIN` documents
            // why a fully transparent backdrop stays recoverable.
            let mut opacity = settings.opacity;
            // Issue #235: `Slider` has no width-builder of its own — its rail is
            // sized entirely off `Spacing::slider_width` (a fixed ~100pt default),
            // unlike the Columns checkboxes and buttons below, which already stretch
            // to the row's available width. This is the only `Slider` in the whole
            // overlay, so widening the shared spacing setting here can't affect
            // anything painted after it.
            ui.spacing_mut().slider_width = ui.available_width();
            let opacity_response = ui.add(
                egui::Slider::new(&mut opacity, Settings::OPACITY_MIN..=Settings::OPACITY_MAX)
                    .show_value(false),
            );
            if opacity_response.changed() {
                // Applied immediately (same frame): `draw_header_menu` mutates the
                // caller's `&mut Settings` in place, and `OverlayApp::ui` reads
                // `self.settings.opacity` fresh when it builds the panel `Frame`
                // right after `draw_header` returns — no extra repaint request
                // needed, unlike an async round trip such as the Share screenshot's.
                settings.set_opacity(opacity);
                // Same persistence path as the Columns checkboxes just above:
                // blocking file IO stays off this render thread, and a dropped
                // writer thread just leaves the in-memory value correct for the
                // rest of this session.
                let _ = tx_settings.send(settings.clone());
            }

            ui.separator();

            // Issues #121 and #253: one labelled section owning both custom
            // background regions. Its own section rather than a nesting inside
            // Columns (which is a per-column list) and rather than two scattered
            // items, because the two rows are the same control twice and read as a
            // pair — and because it sits directly under the Opacity slider that
            // fades both of them, which is the relationship #253 is about.
            ui.label("Background images");
            for slot in ImageSlot::ALL {
                background_image_row(ui, slot, settings, tx_settings, icons);
            }

            ui.separator();

            // Issue #53: this minimize goes to the notification area, not the
            // taskbar. `platform::install_tray`'s subclass intercepts the
            // `WM_SIZE`/`SIZE_MINIMIZED` this command produces, adds a tray icon
            // and hides the window, so no call-site change is needed here — but the
            // tray icon is now the *only* way back, so don't route this through
            // anything that bypasses a real minimize.
            if ui.button("Minimize to tray").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ui.close();
            }

            ui.separator();

            // Issue #203: a UI-settings reset (window size + opacity), distinct
            // from the tray's own OS-level `TrayCommand::ResetWindow` (which
            // recenters/resizes back to the full 20-row raid default and never
            // touches opacity). This resizes to a 5-row sample instead, using the
            // same column set `default_inner_width` already sizes for, and puts
            // opacity back to `Settings::default_opacity()` through the same
            // `set_opacity` + `tx_settings` path the slider above uses.
            //
            // Issue #121 widens what "defaults" means here: this used to put back
            // `opacity` and nothing else, so every field added since issue #203 —
            // the visible-column set, the click-through and pin toggles, the
            // history retention values, and now both custom images — quietly
            // escaped the reset the button's own label promised.
            // `Settings::reset_to_defaults` covers the whole struct, and the image
            // cache is dropped alongside it so the textures for images that are no
            // longer configured are released the same frame rather than lingering
            // until the next paint notices.
            //
            // Still distinct from the header toggle cluster's Reset *icon*, which
            // resets encounter data (`UiCommand::Reset`) and touches no settings at
            // all — issue #121 is explicit that the two must not be confused, which
            // is why this one lives here in the dropdown and says "to defaults".
            if ui.button("Reset to defaults").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    default_inner_width(),
                    reset_to_defaults_inner_height(),
                )));
                settings.reset_to_defaults();
                for slot in ImageSlot::ALL {
                    icons.custom.borrow_mut().clear(slot);
                }
                let _ = tx_settings.send(settings.clone());
                ui.close();
            }

            // Issue #220: a user hitting a bug has no in-app way to hand over the
            // logs `logging::init` already writes for exactly this purpose. Never a
            // fixed or hidden path — the save dialog is what lets the user pick the
            // destination themselves, per the issue.
            //
            // The dialog itself is called inline, on this thread:
            // `platform::choose_log_export_path`'s own doc comment explains why a
            // modal the OS is already blocking on needs no thread. The copy that
            // follows is a different matter — up to two files of
            // `logging::MAX_LOG_BYTES` — so it goes to a spawned thread (PR #227
            // review), the "Check for updates" shape.
            if ui.button("Export logs").clicked() {
                if let Some(dest) =
                    crate::platform::choose_log_export_path(crate::logging::EXPORT_DEFAULT_FILENAME)
                {
                    start_log_export(dest, tx_log_export.clone());
                }
                ui.close();
            }

            // The whole-session handover: logs plus the packet-inspection dump
            // ring (if `SHINRA_INSPECT` was on this session), `settings.json`,
            // and a `manifest.json` describing all of it — everything an agent
            // needs to find a bug without the maintainer's help, in one folder
            // rather than the log-only file "Export logs" above hands over.
            // `crate::bundle`'s module doc comment has the full shape;
            // `history.sqlite` is deliberately never included (it holds
            // plaintext party member names) and `manifest.json` says so.
            //
            // Same dialog-inline / copy-on-a-spawned-thread split as "Export
            // logs" just above, and the same reply channel (see this module's
            // own doc comment for why one channel serves both items).
            if ui.button("Export session bundle").clicked() {
                if let Some(dest) = crate::platform::choose_bundle_export_path(
                    bundle::EXPORT_BUNDLE_DEFAULT_DIRNAME,
                ) {
                    start_bundle_export(dest, tx_log_export.clone());
                }
                ui.close();
            }

            // Issue #214: the only in-process recovery from a capture wedge that no
            // new TCP connection happens to clear. Before this, `request_restart`
            // had no caller anywhere and #211's 24-minute stall could only be
            // escaped by leaving the instance (which opens a fresh connection) or
            // relaunching the app. `bpsr-logs` ships the same escape hatch, worded
            // almost identically — upstream evidently concluded that not every
            // stall condition is automatically detectable.
            //
            // Deliberately unconfirmed and never disabled: the request is a
            // latching flag rather than a queue, so clicking twice is one restart;
            // the cost is a fraction of a second of missed packets plus a decoder
            // reset; and anyone reaching for it is already looking at a meter that
            // has stopped moving.
            if ui.button("Restart packet capture").clicked() {
                let _ = tx_command.try_send(UiCommand::RestartCapture);
                ui.close();
            }

            ui.separator();

            // Issue #171: manual-only, per the issue — there is no automatic or
            // background check anywhere in this crate, only this button. The
            // request itself never touches this thread: clicking spawns a
            // dedicated `std::thread` that calls `update_check::check_for_update`
            // and reports back over a fresh `crossbeam_channel`, the same
            // one-shot-thread shape `settings::spawn_writer` uses for its own
            // (longer-lived) writer thread — see `UpdateCheckState`'s doc comment
            // for why the state lives on `OverlayApp` rather than here.
            //
            // The button stays enabled (and re-clickable) once a check is done,
            // both to let the user retry after a transient network error and to
            // let them re-check right before actually upgrading; it is only
            // disabled while one is already in flight, so a click can't pile up a
            // second thread racing the first to the same channel.
            //
            // Issue #250: the button is also disabled while an install is
            // running or the app is on its way out — re-checking mid-swap
            // would only race the state machine, and a `Restarting` app has
            // nothing left to check.
            let busy = matches!(
                update_check,
                UpdateCheckState::Checking { .. }
                    | UpdateCheckState::Installing { .. }
                    | UpdateCheckState::Restarting
            );
            let clicked_check_for_updates = ui
                .add_enabled(!busy, egui::Button::new("Check for updates"))
                .clicked();
            if clicked_check_for_updates {
                *update_check = start_update_check();
            }
            // Issue #250: an "Update now" click can't assign `*update_check`
            // from inside the match below, which borrows it — so the click
            // is collected here and acted on once the match has ended.
            let mut clicked_install: Option<CheckOutcome> = None;
            match &*update_check {
                UpdateCheckState::Idle => {}
                UpdateCheckState::Checking { .. } => {
                    ui.label("Checking…");
                }
                UpdateCheckState::Done(Ok(CheckOutcome::UpToDate)) => {
                    ui.label(format!("Up to date (v{})", env!("CARGO_PKG_VERSION")));
                }
                UpdateCheckState::Done(Ok(available @ CheckOutcome::UpdateAvailable { .. })) => {
                    draw_update_available(ui, available, &mut clicked_install);
                }
                UpdateCheckState::Done(Err(err)) => {
                    ui.label(format!("Update check failed: {err}"));
                }
                UpdateCheckState::Installing { available, .. } => {
                    let tag = update_tag(available);
                    ui.label(format!("Downloading {tag}…"));
                    // The install thread reports once, at the end — WinHTTP's
                    // read loop has no progress callback wired through
                    // `platform::http_get_bytes` — so this is a spinner, not a
                    // percentage. Claiming a percentage it cannot know would be
                    // worse than not showing one.
                    ui.spinner();
                }
                UpdateCheckState::Restarting => {
                    ui.label("Restarting…");
                }
                UpdateCheckState::InstallFailed { available, error } => {
                    // The offer is redrawn above the error on purpose: a
                    // failed download is usually transient (a dropped
                    // connection, a proxy hiccup), so the retry has to be one
                    // click away rather than behind a fresh check.
                    draw_update_available(ui, available, &mut clicked_install);
                    ui.label(format!("Update failed: {error}"));
                }
            }
            if let Some(available) = clicked_install {
                *update_check = start_update_install(available);
            }

            ui.separator();

            if ui
                .add(menu_item_button(
                    icons.toolbar.get(ToolbarIcon::Close),
                    "Close",
                ))
                .clicked()
            {
                let _ = tx_command.try_send(UiCommand::Quit);
                *quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ui.close();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
    // -- toolbar icon tint (half-white, matching the source) --------------

    /// `toolbar_icon_image` must multiply every toolbar/stat icon by the
    /// source's half-white tint instead of leaving the source PNG's native
    /// color untouched.
    #[test]
    fn toolbar_icon_image_applies_the_half_white_tint() {
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "test-icon-tint",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );
        let image = toolbar_icon_image(&texture);
        assert_eq!(image.image_options().tint, TOOLBAR_ICON_TINT);
    }

    /// Issue #186: the History button opens the history view when there is
    /// a history thread, and is inert — not merely dim — when there isn't,
    /// which is the disabled state the `draw_header_menu` item it replaced
    /// had via `add_enabled`.
    #[test]
    fn clicking_history_opens_it_only_when_history_is_available() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let clicked = |has_history: bool, label: &str| -> bool {
            let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
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
            let update = layout
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            let pos = accessible_rect_for_label(&update, label).center();
            layout.drop_without_applying_deltas();

            let mut open_history = false;
            ctx.run_ui(click_at(pos), |ui| {
                toggle_cluster(
                    ui,
                    &tx_command,
                    &icons,
                    false,
                    true,
                    has_history,
                    &mut open_history,
                );
            })
            .drop_without_applying_deltas();
            open_history
        };

        assert!(
            clicked(true, "History"),
            "History must open the saved-encounter list"
        );
        assert!(
            !clicked(false, "History: unavailable"),
            "History must stay inert with no history thread"
        );
    }

    /// A row too narrow for the controls degrades to an empty pill inside
    /// the row rather than an inverted one, exactly like `chevron_rect` and
    /// `header_text_rect` do.
    #[test]
    fn the_title_toggle_pill_degrades_at_an_absurd_width() {
        let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(4.0, 22.0));
        let pill = title_toggle_pill_rect(row, 18.0);
        assert!(pill.width() >= 0.0, "{pill:?} is inverted");
        assert!(pill.left() >= row.left() && pill.right() <= row.right());
    }

    /// A display shorter than the margin still gets a non-negative cap —
    /// egui's `ScrollArea::max_height` on a negative value would otherwise
    /// invert the scroll area rather than just shrinking it to nothing.
    #[test]
    fn header_menu_scroll_max_height_never_goes_negative() {
        assert_eq!(header_menu_scroll_max_height(10.0, 24.0), 0.0);
    }

    // -- background-image status line (issues #121, #253) -----------------

    #[test]
    fn background_image_status_shows_the_file_name_when_it_loaded() {
        assert_eq!(
            background_image_status(Path::new("C:/Users/x/Pictures/wallpaper.png"), None),
            "wallpaper.png"
        );
    }

    /// The whole point of the status line: a path that does not load has to
    /// say so, and say *why*, or the feature just looks broken.
    #[test]
    fn background_image_status_names_the_failure_when_it_did_not_load() {
        let status = background_image_status(
            Path::new("C:/gone.png"),
            Some(&ImageError::Unreadable("os error 2".to_string())),
        );
        assert!(status.contains("gone.png"), "{status}");
        assert!(status.contains("could not be read"), "{status}");
        assert!(status.contains("os error 2"), "{status}");
        assert!(status.starts_with('⚠'), "{status}");

        let status = background_image_status(
            Path::new("notes.txt"),
            Some(&ImageError::Undecodable("unsupported".to_string())),
        );
        assert!(status.contains("not a readable image"), "{status}");
    }

    /// A path with no file-name component (a bare root, or one ending in
    /// `..`) must still produce a label rather than an empty one.
    #[test]
    fn background_image_status_falls_back_to_the_whole_path_without_a_file_name() {
        assert!(!background_image_status(Path::new("/"), None).is_empty());
        assert!(!background_image_status(Path::new("../.."), None).is_empty());
    }

    /// Regression test for the stale-error mixup this PR fixes: pick a
    /// path that fails to load, then re-pick a *different*, valid path.
    /// The status line for the new path must never carry the old path's
    /// failure — `CustomImages::error` is what `background_image_row`
    /// reads to build that line, and it must reject a cached entry whose
    /// own `path` no longer matches the one being asked about, exactly as
    /// happens for one frame between a re-pick and the next `texture()`
    /// call re-keying the cache.
    #[test]
    fn background_image_status_never_attributes_a_stale_error_to_a_different_path() {
        let ctx = egui::Context::default();
        let mut cache = CustomImages::default();
        let bad = std::env::temp_dir().join("shinra-ui-status-mismatch-missing.png");
        let _ = std::fs::remove_file(&bad);

        // First pick: a path that fails to load. This caches an `Err` entry
        // keyed on `bad`, exactly like a real failed pick.
        assert!(
            cache
                .texture(&ctx, ImageSlot::Header, &bad, [64, 32])
                .is_none()
        );
        let status = background_image_status(&bad, cache.error(ImageSlot::Header, &bad).as_ref());
        assert!(status.starts_with('⚠'), "{status}");
        assert!(
            status.contains("shinra-ui-status-mismatch-missing.png"),
            "{status}"
        );

        // Second pick: a different, valid path. Settings now reports the
        // new path, but nothing has called `texture()` for it yet (that
        // only happens once the header/backdrop is actually painted) — so
        // the row's very next frame reads `error` against a cache entry
        // that is still keyed on `bad`. That must not surface as an error
        // for `good`.
        let good = std::env::temp_dir().join("shinra-ui-status-mismatch-good.png");
        let error = cache.error(ImageSlot::Header, &good);
        assert!(
            error.is_none(),
            "a stale entry for a different path must not be reported: {error:?}"
        );
        let status = background_image_status(&good, error.as_ref());
        assert_eq!(
            status, "shinra-ui-status-mismatch-good.png",
            "the new path's status must not mention the old path's failure"
        );
        assert!(!status.contains('⚠'), "{status}");
    }

    // -- toolbar/menu icons (issue #41, #71) -------------------------------

    /// `TOOLBAR_ICON_SIZE` plus `button_padding` must land exactly on
    /// `interact_size.y` — `CHEVRON_SIZE` is defined off this, so a drift
    /// here would desync the chevron's hit box from the rest of the theme.
    #[test]
    fn toolbar_icon_button_height_matches_interact_size() {
        // `apply_theme` is what actually sets `button_padding` at runtime
        // (`egui::Style::default()`'s own padding is a different value) —
        // exercise that, not the untouched default, or this test would
        // check a style `draw_header` never actually runs under.
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let mut padding = egui::Vec2::ZERO;
        let mut interact_size_y = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            padding = ui.spacing().button_padding;
            interact_size_y = ui.spacing().interact_size.y;
        })
        .drop_without_applying_deltas();
        let button_height = TOOLBAR_ICON_SIZE + 2.0 * padding.y;
        assert_eq!(button_height, interact_size_y);
    }

    /// `menu_item_button` must fall back to a plain text button — not paint
    /// nothing or panic — when the icon texture failed to decode
    /// (belt-and-braces, mirrors `ClassIcons::get`'s `None` case).
    #[test]
    fn menu_item_button_falls_back_to_text_when_texture_is_none() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = ui.add(menu_item_button(None, "Close"));
            // A real widget was allocated — a non-zero rect — not a no-op.
            assert!(response.rect.width() > 0.0);
            assert!(response.rect.height() > 0.0);
        })
        .drop_without_applying_deltas();
    }

    /// Unlike the old image-only `icon_button`, `menu_item_button` always
    /// carries a text atom (`Button::image_and_text`/`Button::new`), so its
    /// accessible name comes for free from the button's own text — no
    /// hand-rolled `widget_info` call needed the way the old image-only
    /// buttons required. Regression guard for that assumption.
    #[test]
    fn menu_item_button_with_a_texture_has_an_accessible_label_matching_the_text() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let texture = ctx.load_texture(
            "test-icon",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );

        let mut id = egui::Id::NULL;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            id = ui.add(menu_item_button(Some(&texture), "Close")).id;
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let label = accessible_label(&update, id);
        output.drop_without_applying_deltas();

        assert_eq!(label.as_deref(), Some("Close"));
    }

    /// Regression coverage for issue #231's actual fix
    /// (`ui.set_max_height(scroll_max_height)` in `draw_header_menu`, just
    /// above the `ScrollArea`), which no purely-arithmetic test on
    /// `header_menu_scroll_max_height` can see: the bug it fixes is a
    /// cross-frame feedback loop in `egui::Popup`'s underlying `Area`,
    /// which remembers its previous rendered rect and hands that back as
    /// next frame's available size (see `draw_header_menu`'s doc comment
    /// on `set_max_height`). That loop only exists across multiple frames
    /// of the *same* `Area` state, so — like
    /// `header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close`
    /// just above — this drives the real `draw_header` (chevron, popup
    /// wiring, and all) rather than calling `draw_header_menu` directly.
    ///
    /// The popup's actual on-screen rect is read back via
    /// `egui::Memory::area_rect`, keyed by the same
    /// `Popup::default_response_id(&chevron_response)` id production code
    /// uses — reconstructed here from the chevron's accesskit `NodeId`
    /// (`Id::accesskit_id` is a direct, unhashed wrap of `Id::value()`, so
    /// `Id::from_high_entropy_bits` undoes it exactly; that round trip is
    /// the documented purpose of that method) since `draw_header` has no
    /// other way to hand a private `Response` out to a test.
    ///
    /// A screen with plenty of headroom is used deliberately (rather than
    /// one short enough to force scrolling): without the fix, removing
    /// `ui.set_max_height` doesn't make the popup overflow the cap — it
    /// makes it get stuck at its pre-expansion (Columns collapsed) height
    /// forever, since the `Area`'s remembered small rect keeps being fed
    /// back as this `Ui`'s `max_rect` on every later frame and the
    /// `ScrollArea`'s own `max_height` can't grow a `Ui` past what it was
    /// given. A generous screen isolates that stuck-small failure from the
    /// cap itself, which this test also checks holds regardless.
    #[test]
    fn header_menu_popup_grows_to_fit_columns_once_expanded() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        // Same reasoning as
        // `header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close`:
        // a zero animation time makes the Columns disclosure arrow (and
        // its checkboxes) snap straight to fully open within the same
        // frame as the click, instead of animating in over several.
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(0);
        let mut gesture = WindowGesture::default();
        let mut update_check = UpdateCheckState::default();

        // Tall enough that the fully expanded menu (all nine of
        // `ColumnKind::ALL`'s checkboxes) fits with room to spare — see
        // the doc comment above for why a generous screen, not a short
        // one, is what actually isolates this regression.
        let screen_height = 1000.0;
        let scroll_max_height =
            header_menu_scroll_max_height(screen_height, HEADER_MENU_SCROLL_MARGIN);

        let mut frame = |mut input: egui::RawInput| -> egui::accesskit::TreeUpdate {
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(default_inner_width(), screen_height),
            ));
            let output = ctx.run_ui(input, |ui| {
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
                    &mut gesture,
                    false,
                    true,
                    &mut update_check,
                    &unused_log_export_sender(),
                    false,
                    &mut false,
                    None,
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
        };
        let popup_height = |popup_id: egui::Id| {
            ctx.memory(|mem| mem.area_rect(popup_id))
                .expect("the header menu popup must have a recorded Area rect by now")
                .height()
        };

        // Frame 1: closed header, find the chevron and its accesskit id.
        let update = frame(egui::RawInput::default());
        let chevron_node_id = update
            .nodes
            .iter()
            .find_map(|(node_id, node)| {
                node.label()
                    .is_some_and(|s| s == "Menu")
                    .then_some(*node_id)
            })
            .expect("no accessible node labeled \"Menu\" painted");
        let chevron_pos = accessible_rect_for_label(&update, "Menu").center();
        // SAFETY: `chevron_node_id.0` is exactly the `u64` `Id::value()`
        // this node's accesskit id was derived from (`Id::accesskit_id`
        // performs no hashing, only a direct wrap) — recovering that same
        // `Id` from it is `Id::from_high_entropy_bits`'s documented use
        // case, not a hash collision gamble.
        let chevron_id = unsafe { egui::Id::from_high_entropy_bits(chevron_node_id.0) };
        // Matches `Popup::default_response_id`, which is exactly what
        // `Popup::menu(&chevron_response)` (via `draw_header`) keys its
        // `Area`'s remembered rect under.
        let popup_id = chevron_id.with("popup");

        // Frame 2: open the menu. Its *position* isn't trustworthy yet —
        // same reasoning as
        // `header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close`
        // — so this only checks that it opened at all, via "Close" (always
        // present, regardless of Columns' collapsed/expanded state).
        let update = frame(click_at(chevron_pos));
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Close")),
            "clicking the chevron must open the menu"
        );

        // Frame 3: let the just-opened popup settle out of its first,
        // sizing-only pass into a stable position — same reasoning as
        // `header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close`.
        let update = frame(egui::RawInput::default());
        let columns_pos = accessible_rect_for_label(&update, "Columns").center();
        let collapsed_height = popup_height(popup_id);
        assert!(
            collapsed_height <= scroll_max_height,
            "collapsed height {collapsed_height} must already be within the \
             {scroll_max_height} cap"
        );

        // Frame 4: expand Columns — every one of its nine checkboxes now
        // needs somewhere to go.
        let _ = frame(click_at(columns_pos));

        // Several more settle frames with no further input: this is
        // exactly where issue #231's feedback loop lived. Without
        // `ui.set_max_height`, the `Area`'s remembered pre-expansion rect
        // kept being handed straight back to this `Ui` as its `max_rect`
        // on every one of these frames, and the newly expanded content
        // never had room to register as taller — the popup stayed frozen
        // at `collapsed_height` no matter how many frames passed.
        let mut heights = Vec::new();
        for _ in 0..5 {
            let _ = frame(egui::RawInput::default());
            heights.push(popup_height(popup_id));
        }

        for (frame_index, height) in heights.iter().enumerate() {
            assert!(
                *height <= scroll_max_height,
                "settle frame {frame_index}: popup height {height} exceeded the \
                 {scroll_max_height} cap across all settle frames {heights:?}"
            );
        }
        // The actual regression: with the bug, every one of these stays
        // pinned at `collapsed_height` (a ~7px difference, just the
        // Columns arrow's own rotation) instead of growing to fit nine
        // freshly revealed checkboxes.
        let expanded_height = *heights.last().unwrap();
        assert!(
            expanded_height > collapsed_height + 50.0,
            "expanding Columns must grow the popup well past its collapsed \
             height {collapsed_height} to fit all nine checkboxes, got \
             {heights:?}"
        );
        // And it must reach that size promptly, not keep climbing frame
        // after frame once the popup has had several frames to settle.
        let peak = heights.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            expanded_height >= peak - 1.0,
            "popup height must not still be growing after several settle \
             frames: {heights:?}"
        );
    }

    /// Its hit box is a `TOOLBAR_ICON_SIZE` square, the same footprint the
    /// dropdown's own menu-item icons use, so it is as easy to hit as they
    /// are.
    #[test]
    fn the_chevron_hit_box_is_a_toolbar_sized_square() {
        let chevron = chevron_rect(title_row());
        assert_eq!(chevron.width(), CHEVRON_SIZE);
        assert_eq!(chevron.height(), CHEVRON_SIZE);
        assert_eq!(CHEVRON_SIZE, TOOLBAR_ICON_SIZE);
    }

    /// An absurdly narrow row degrades to a small (never inverted) box
    /// inside the row, the same way `header_text_rect` degrades to an empty
    /// text rect.
    #[test]
    fn the_chevron_never_inverts_in_a_hopelessly_narrow_row() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(8.0, 20.0));
        let chevron = chevron_rect(row);
        assert!(chevron.width() >= 0.0);
        assert!(row.contains_rect(chevron));
    }

    /// `chevron_points` still supports mirroring the V about the box's
    /// center line for either direction — `menu_chevron` itself now only
    /// ever asks for `true` (down, a menu affordance, never a
    /// collapse-state indicator; see `the_chevron_always_points_down`), but
    /// the pure function underneath stays general.
    #[test]
    fn chevron_points_mirrors_up_and_down_about_its_center() {
        let rect = chevron_rect(title_row());
        let down = chevron_points(rect, true);
        let up = chevron_points(rect, false);

        // The tip is the middle point; the two ends sit opposite it.
        assert!(down[1].y > down[0].y, "expanded should point down");
        assert!(up[1].y < up[0].y, "collapsed should point up");
        assert_eq!(down[0].x, up[0].x);
        assert_eq!(down[2].x, up[2].x);
        for (a, b) in down.iter().zip(up.iter()) {
            assert!(
                (a.y - rect.center().y) + (b.y - rect.center().y) < 0.001,
                "the two states should mirror about the box's center"
            );
        }
    }

    /// The V is wide and shallow, matching the source's `Width="10"` chevron
    /// rather than an arrowhead, and it stays inside its box.
    #[test]
    fn the_chevron_is_a_wide_shallow_v_inside_its_box() {
        let rect = chevron_rect(title_row());
        let points = chevron_points(rect, true);
        for point in &points {
            assert!(rect.contains(*point), "{point:?} escapes {rect:?}");
        }
        let width = points[2].x - points[0].x;
        let depth = points[1].y - points[0].y;
        assert_eq!(width, CHEVRON_PAINT_WIDTH);
        assert_eq!(depth, CHEVRON_PAINT_HEIGHT);
        assert!(width >= depth * 2.0, "{width}pt wide vs {depth}pt deep");
    }

    /// Same accessibility regression the old `minimize_button` guarded
    /// against: a raw `interact` response carries no `WidgetInfo` from
    /// anywhere, so without the explicit call a screen-reader user hears an
    /// unlabeled control. Unlike the old collapse chevron, the label no
    /// longer flips with any state — it always names the menu affordance.
    #[test]
    fn the_chevron_has_an_accessible_label_naming_the_menu() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();

        let mut id = egui::Id::NULL;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            id = menu_chevron(ui, chevron_rect(title_row())).id;
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let label = accessible_label(&update, id);
        output.drop_without_applying_deltas();

        assert_eq!(label.as_deref(), Some("Menu"));
    }

    /// `menu_chevron` always points down (issue #71) — it is a menu
    /// affordance, not a collapse-state indicator, so it must not flip when
    /// the overlay is collapsed.
    #[test]
    fn the_chevron_always_points_down() {
        let rect = chevron_rect(title_row());
        let points = chevron_points(rect, true);
        // The tip is the middle point; the two ends sit opposite it, above
        // the tip when pointing down.
        assert!(points[1].y > points[0].y);
        assert!(points[1].y > points[2].y);
    }
}
