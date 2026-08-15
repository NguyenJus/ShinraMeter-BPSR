//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in.

use std::time::Duration;

use bpsr_meter::{EncounterInfo, PlayerRow, Snapshot};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::icons::{ClassIcons, ToolbarIcon, ToolbarIcons};
use crate::settings::{ColumnKind, Settings};

/// Commands the overlay emits for the app layer to consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    Reset,
    Quit,
}

/// Non-fatal status banner shown above the player rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusLine {
    Ok,
    Error(String),
}

/// The overlay's eframe app: holds the latest snapshot plus the channel
/// endpoints used to receive updates and send commands.
pub struct OverlayApp {
    snapshot: Snapshot,
    status: StatusLine,
    settings: Settings,
    rx_snapshot: Receiver<Snapshot>,
    tx_command: Sender<UiCommand>,
    tx_settings: Sender<Settings>,
    /// Class icon and toolbar icon textures (issues #9, #41), bundled behind
    /// one `Option` so there is a single lazy-init site (see `Icons::load`)
    /// rather than one per icon set. `None` until the first `ui()` call —
    /// texture loading needs an `egui::Context`, which does not exist yet at
    /// `OverlayApp::new` — then loaded exactly once for the process's life.
    icons: Option<Icons>,
}

/// All icon textures the overlay paints, bundled so `OverlayApp` has exactly
/// one lazily-loaded field for them instead of one per icon set (issue #41).
struct Icons {
    classes: ClassIcons,
    toolbar: ToolbarIcons,
}

impl Icons {
    fn load(ctx: &egui::Context) -> Self {
        Self {
            classes: ClassIcons::load(ctx),
            toolbar: ToolbarIcons::load(ctx),
        }
    }
}

impl OverlayApp {
    /// `settings` is loaded by the caller (`main.rs`) rather than via
    /// `settings::load()` in here, because issue #27 needs the same loaded
    /// value before this exists too — to build `ui::viewport`'s starting
    /// position — so there is exactly one load per run, not one for the
    /// viewport and a second (redundant, potentially racing a concurrent
    /// write) one here.
    pub fn new(
        rx_snapshot: Receiver<Snapshot>,
        tx_command: Sender<UiCommand>,
        tx_settings: Sender<Settings>,
        settings: Settings,
    ) -> Self {
        Self {
            snapshot: Snapshot {
                duration_ms: 0,
                total_damage: 0,
                total_dps: 0.0,
                rows: Vec::new(),
                encounter: EncounterInfo::default(),
            },
            status: StatusLine::Ok,
            settings,
            rx_snapshot,
            tx_command,
            tx_settings,
            icons: None,
        }
    }

    pub fn with_status(mut self, status: StatusLine) -> Self {
        self.status = status;
        self
    }
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drain the channel, keeping only the most recent snapshot.
        for snap in self.rx_snapshot.try_iter() {
            self.snapshot = snap;
        }

        let ctx = ui.ctx().clone();
        apply_theme(&ctx);

        // Loaded once, lazily: the `egui::Context` above isn't available yet
        // at `OverlayApp::new`, so the first frame is what actually uploads
        // the icon textures (issues #9, #41); every later frame reuses them.
        let icons = self.icons.get_or_insert_with(|| Icons::load(&ctx));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default().fill(egui::Color32::from_rgba_unmultiplied(18, 18, 22, 200)),
            )
            .show(ui, |ui| {
                // First, so the header buttons drawn afterwards stay on top of
                // the corner zones they overlap.
                draw_resize_handles(ui, &ctx);
                draw_header(
                    ui,
                    &ctx,
                    &self.snapshot,
                    &self.tx_command,
                    &mut self.settings,
                    &self.tx_settings,
                    &icons.toolbar,
                );

                if let StatusLine::Error(msg) = &self.status {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), msg.as_str());
                }

                ui.separator();
                draw_rows(
                    ui,
                    &self.snapshot,
                    &self.settings.ordered_columns(),
                    &icons.classes,
                );
            });

        track_window_position(&ctx, &mut self.settings, &self.tx_settings);

        // ~10 Hz.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Tracks the window's outer (on-screen) position and persists it via the
/// same settings-writer path column settings use (issue #27). `outer_rect`
/// is reported on every single frame — including every frame of a drag
/// gesture — so `Settings::with_window_position_if_changed` gates the send
/// on an actual change; the writer thread's own burst-coalescing
/// (`run_writer` in `settings.rs`) still collapses a drag's many small
/// changes into a single disk write.
///
/// A minimized window is skipped entirely: the platform parks it far
/// off-screen (Windows uses -32000, -32000) and reports *that* as the outer
/// position, which would otherwise be persisted and reopen the overlay
/// somewhere the user cannot reach it.
fn track_window_position(
    ctx: &egui::Context,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
) {
    let (outer_rect, minimized) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().minimized));
    if minimized == Some(true) {
        return;
    }
    let Some(rect) = outer_rect else {
        return;
    };
    if !is_plausible_position(rect.min) {
        return;
    }
    if let Some(updated) = settings.with_window_position_if_changed([rect.min.x, rect.min.y]) {
        *settings = updated.clone();
        let _ = tx_settings.send(updated);
    }
}

/// Whether a reported window position is worth persisting at all — belt and
/// braces behind `track_window_position`'s minimized guard, in case a
/// platform reports its off-screen parking spot for a frame before the
/// `minimized` flag catches up. A multi-monitor layout can legitimately put
/// the overlay at negative coordinates, so only absurd ones are rejected.
fn is_plausible_position(position: egui::Pos2) -> bool {
    position.x.is_finite()
        && position.y.is_finite()
        && position.x > MIN_PLAUSIBLE_COORD
        && position.y > MIN_PLAUSIBLE_COORD
}

/// Floor for a believable on-screen coordinate: far enough out to clear any
/// real monitor arrangement, tight enough to catch Windows' -32000 parking
/// spot for minimized windows.
const MIN_PLAUSIBLE_COORD: f32 = -20_000.0;

fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
    toolbar: &ToolbarIcons,
) {
    let title = encounter_title(&snapshot.encounter);
    let subtitle = encounter_subtitle(&snapshot.encounter);

    // The whole header band is the drag surface — title line, the optional
    // subtitle line, and the timer/DPS/buttons row — registered *before* the
    // row's contents so the buttons drawn into it end up on top and still get
    // their clicks. Grabbing a single glyph was too small a target to hit.
    let band = {
        let mut rect = ui.available_rect_before_wrap();
        let height = header_band_height(subtitle.is_some(), ui.spacing().interact_size.y);
        rect.max.y = rect.min.y + height;
        // Leave the top resize strip alone — a drag surface spanning it would
        // win the hit test and swallow every north-edge resize.
        rect.min.y += RESIZE_EDGE;
        rect
    };
    let drag_surface = ui.interact(band, ui.id().with("title_bar"), egui::Sense::drag());
    if drag_surface.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::Grab);
    }
    // Once per gesture: `drag_window` starts a modal move loop on the OS side,
    // so re-sending it every frame while the drag is held is at best redundant.
    if drag_surface.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    // Title is always rendered (even as the "No target" placeholder) so the
    // header's height never jitters between frames; the subtitle is omitted
    // entirely — not rendered blank — when the scene is unknown (issue #9
    // slice 2).
    draw_title_line(ui, &title);
    if let Some(subtitle) = &subtitle {
        draw_subtitle_line(ui, subtitle);
    }

    ui.horizontal(|ui| {
        // Purely an affordance — the band above is what actually drags.
        ui.label("☰");

        // Decorative — painted immediately left of the duration text, no
        // click target and no tooltip of its own (issue #41). Skipped
        // entirely if the PNG somehow failed to decode, same as a row's
        // class icon (`draw_row`) skips painting rather than leaving a
        // broken-image placeholder.
        if let Some(clock) = toolbar.get(ToolbarIcon::Clock) {
            ui.add(toolbar_icon_image(clock));
        }
        ui.label(fmt_duration(snapshot.duration_ms));
        ui.label(format!("{} DPS", fmt_short(snapshot.total_dps as i64)));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Raster PNG icons throughout this row (issue #41), not glyphs —
            // neither vendored `epaint_default_fonts` TTF covers a close
            // ✕/gear ⚙/etc. glyph (issue #14's tofu-square problem), the
            // same reason the old "×"/"S" glyphs here were themselves picked
            // for font coverage rather than looks.
            if icon_button(ui, toolbar.get(ToolbarIcon::Close), "×", "Close").clicked() {
                let _ = tx_command.try_send(UiCommand::Quit);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // No minimize icon exists in the upstream ShinraMeter icon set
            // this issue draws from (issue #41's scope note), so this one is
            // painted procedurally — a short horizontal line — rather than
            // left as the old "_" glyph or reusing an unrelated asset.
            //
            // There's no tray icon and no other in-app restore path: this
            // relies entirely on the OS taskbar entry to un-minimize.
            // `viewport()` below never calls `.with_taskbar(false)` (which
            // is what would hide it via `skip_taskbar`), so the window
            // keeps the default winit/OS taskbar presence even though it's
            // borderless and always-on-top. If `viewport()` ever gains a
            // taskbar-hiding or tool-window setting, this button needs a
            // real restore mechanism first.
            if minimize_button(ui).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if icon_button(ui, toolbar.get(ToolbarIcon::Reset), "Reset", "Reset").clicked() {
                let _ = tx_command.try_send(UiCommand::Reset);
            }
            draw_settings_menu(
                ui,
                settings,
                tx_settings,
                toolbar.get(ToolbarIcon::Settings),
            );
        });
    });
}

/// Fixed display size, in points, every toolbar icon (issue #41) is drawn
/// at — independent of the source PNGs' own resolution (48x48 in the
/// upstream ShinraMeter set), so a texture swap can never change a button's
/// footprint. Chosen to land `icon_button`'s total height (icon plus
/// `apply_theme`'s `button_padding.y` on both sides) exactly on
/// `egui::Style::default().spacing.interact_size.y` (18.0), the same height
/// `header_band_height` already budgets for the button row — see
/// `north_strip_is_not_swallowed_by_the_header`-style header-band tests, and
/// this module's own `toolbar_icon_button_height_matches_interact_size`.
const TOOLBAR_ICON_SIZE: f32 = 14.0;

/// Builds an `egui::Image` for a loaded toolbar icon texture at the fixed
/// `TOOLBAR_ICON_SIZE`, overriding whatever size the source PNG itself
/// carries (`SizedTexture::from_handle` would use the PNG's native 48x48
/// instead).
fn toolbar_icon_image(handle: &egui::TextureHandle) -> egui::Image<'static> {
    egui::Image::from_texture(egui::load::SizedTexture::new(
        handle.id(),
        egui::Vec2::splat(TOOLBAR_ICON_SIZE),
    ))
}

/// Paints one toolbar icon button and attaches its tooltip in one place
/// (issue #41's "meaning is not lost" requirement) so every call site gets
/// both without repeating either. Falls back to `fallback_glyph` — the
/// original text/glyph button this replaces — when `texture` is `None`
/// (belt-and-braces: `ToolbarIcons`' bytes are compile-time constants,
/// never actually expected to fail to decode, same reasoning as
/// `ClassIcons::get`).
fn icon_button(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    fallback_glyph: &str,
    tooltip: &str,
) -> egui::Response {
    let response = match texture {
        Some(handle) => ui.add(egui::Button::image(toolbar_icon_image(handle))),
        None => ui.button(fallback_glyph),
    };
    response.on_hover_text(tooltip)
}

/// Paints the minimize button: no icon asset for it exists in the upstream
/// ShinraMeter set (issue #41), so this draws a short horizontal line with
/// `ui.painter()` directly rather than via a texture. The allocated rect
/// matches `icon_button`'s footprint exactly — `TOOLBAR_ICON_SIZE` plus
/// `apply_theme`'s `button_padding` on all sides — so this button doesn't
/// stand out as a different size in the row, and `header_band_height` stays
/// correct without special-casing it.
fn minimize_button(ui: &mut egui::Ui) -> egui::Response {
    let padding = ui.spacing().button_padding;
    let size = egui::Vec2::splat(TOOLBAR_ICON_SIZE) + 2.0 * padding;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let half_width = TOOLBAR_ICON_SIZE / 2.0;
        let y = rect.center().y;
        ui.painter().line_segment(
            [
                egui::pos2(rect.center().x - half_width, y),
                egui::pos2(rect.center().x + half_width, y),
            ],
            egui::Stroke::new(1.5, visuals.fg_stroke.color),
        );
    }
    response.on_hover_text("Minimize")
}

/// Header title text (issue #9 slice 2; gated to boss fights by issue #42):
/// the boss name when the current target is a recognized boss with a
/// resolved name, `Monster #{id}` when it's a recognized boss whose name
/// didn't resolve (the two vendored lists aren't guaranteed to agree — see
/// `EncounterInfo::is_boss`), else blank for a non-boss pull, else "No
/// target" when nothing has been hit yet. Always returns something (never
/// omits the line) so `draw_header` can render it unconditionally and the
/// header's height never jitters between frames depending on whether a
/// target — or a name for it — is known.
///
/// "No target" is kept for the genuinely-empty-encounter case (no target at
/// all), but a non-boss pull is a real target we're deliberately not naming
/// — showing `Monster #{id}` there was dropped rather than kept as the
/// non-boss fallback, since a raw id would read as an unresolved boss name
/// rather than the intentional omission it actually is (the reference meter
/// only names boss fights; see `tables::is_boss_monster`). A *recognized
/// boss* with no resolved name is different: it's a real boss fight, and an
/// empty header would be indistinguishable from a trash pull — so that case
/// still falls back to the raw id.
fn encounter_title(e: &EncounterInfo) -> String {
    match e.boss_monster_id {
        None => "No target".to_string(),
        Some(id) if e.is_boss => e
            .boss_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("Monster #{id}")),
        Some(_) => String::new(),
    }
}

/// Header subtitle text (issue #9 slice 2): the scene name when known, else
/// its raw scene id, else `None` — `draw_header` omits the subtitle line
/// entirely in that case rather than reserving space for nothing.
fn encounter_subtitle(e: &EncounterInfo) -> Option<String> {
    match (e.scene_name, e.scene_id) {
        (Some(name), _) => Some(name.to_string()),
        (None, Some(id)) => Some(format!("Scene #{id}")),
        (None, None) => None,
    }
}

/// Height of the header's title line, reused by both `draw_header`'s
/// drag-band sizing and `default_inner_height` so they can't drift apart —
/// the same pattern `ROW_HEIGHT` follows for player rows.
const TITLE_LINE_HEIGHT: f32 = 20.0;
/// Font size for the title line — larger than the default row text, full-
/// strength colour, matching the reference screenshot (issue #9 slice 2).
const TITLE_FONT_SIZE: f32 = 15.0;

/// Height of the header's subtitle line. Not part of `default_inner_height`
/// — the subtitle is conditional and the default window assumes it is
/// absent (see `default_inner_height`'s doc).
const SUBTITLE_LINE_HEIGHT: f32 = 16.0;
/// Font size for the subtitle line — smaller than the title, dimmed via
/// `ui.visuals().weak_text_color()` rather than a new hard-coded colour.
const SUBTITLE_FONT_SIZE: f32 = 11.0;

/// Height of `draw_header`'s drag band: the title line, the optional
/// subtitle line, and the button row (`button_row_height`, egui's
/// `interact_size.y`), plus one `ITEM_SPACING_Y` gap for every adjacent pair
/// egui's vertical layout stacks them as — 1 gap (title -> button row) when
/// there's no subtitle, 2 (title -> subtitle -> button row) when there is.
/// Extracted from `draw_header` so the two cases are unit-testable without a
/// live `egui::Ui`.
fn header_band_height(has_subtitle: bool, button_row_height: f32) -> f32 {
    let gap_count = if has_subtitle { 2 } else { 1 };
    let mut height = TITLE_LINE_HEIGHT + button_row_height + gap_count as f32 * ITEM_SPACING_Y;
    if has_subtitle {
        height += SUBTITLE_LINE_HEIGHT;
    }
    height
}

/// Paints the header's title line (boss name/id/placeholder) at a fixed
/// height so `draw_header`'s drag band and `default_inner_height` can both
/// reason about it exactly, the same way `draw_row` paints stat text inside
/// an `allocate_exact_size`d rect instead of an auto-sized `ui.label`.
fn draw_title_line(ui: &mut egui::Ui, text: &str) {
    let desired_size = egui::vec2(ui.available_width(), TITLE_LINE_HEIGHT);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(TITLE_FONT_SIZE),
        ui.visuals().text_color(),
    );
}

/// Paints the header's subtitle line (scene name/id), dimmed. Only called
/// when `encounter_subtitle` returned `Some` — the caller skips this
/// entirely, rather than calling it with empty text, so no space is
/// reserved when the scene is unknown.
fn draw_subtitle_line(ui: &mut egui::Ui, text: &str) {
    let desired_size = egui::vec2(ui.available_width(), SUBTITLE_LINE_HEIGHT);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(SUBTITLE_FONT_SIZE),
        ui.visuals().weak_text_color(),
    );
}

/// The settings menu: a compact dropdown (egui's `menu_button`/
/// `menu_image_button`, so it needs no extra open/closed state of its own)
/// letting the user toggle which stat columns render (issue #13). The
/// trigger is the gear icon (issue #41) when its texture decoded, else the
/// original `"S"` glyph — same fallback `icon_button` uses, but
/// `menu_button`/`menu_image_button` aren't unifiable behind one helper the
/// way plain buttons are (they return different widget types), so the
/// `match` is inlined here instead.
fn draw_settings_menu(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
    icon: Option<&egui::TextureHandle>,
) {
    let add_contents = |ui: &mut egui::Ui| {
        ui.label("Columns");
        let mut changed = false;
        for col in ColumnKind::ALL {
            let is_visible = settings.is_visible(col);
            // Disabling the last remaining column would leave the row with
            // nothing to show, so its checkbox is greyed out and inert
            // rather than letting the click land (issue #13's "keep the
            // UI usable" guard).
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
            // value to the dedicated settings-writer thread instead, same
            // as `pipeline::spawn` owns the meter off this thread. A
            // disconnected receiver (writer thread gone) is not fatal:
            // the in-memory `settings` the UI already mutated stays
            // correct for the rest of this session.
            let _ = tx_settings.send(settings.clone());
        }
    };

    let trigger = match icon {
        Some(handle) => {
            ui.menu_image_button(toolbar_icon_image(handle), add_contents)
                .response
        }
        None => ui.menu_button("S", add_contents).response,
    };
    trigger.on_hover_text("Settings");
}

/// Width of the invisible edge strips that start a resize, in points.
const RESIZE_EDGE: f32 = 6.0;
/// Side of the invisible corner squares, which resize on both axes at once.
const RESIZE_CORNER: f32 = 14.0;

/// The eight grab zones around a window rect, edges first so the corners
/// registered after them win where the two overlap.
fn resize_zones(rect: egui::Rect) -> [(egui::Rect, egui::ResizeDirection, egui::CursorIcon); 8] {
    use egui::{CursorIcon as Cursor, Rect, ResizeDirection as Dir, pos2};

    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let (e, c) = (RESIZE_EDGE, RESIZE_CORNER);

    [
        (
            Rect::from_min_max(pos2(l, t), pos2(r, t + e)),
            Dir::North,
            Cursor::ResizeNorth,
        ),
        (
            Rect::from_min_max(pos2(l, b - e), pos2(r, b)),
            Dir::South,
            Cursor::ResizeSouth,
        ),
        (
            Rect::from_min_max(pos2(l, t), pos2(l + e, b)),
            Dir::West,
            Cursor::ResizeWest,
        ),
        (
            Rect::from_min_max(pos2(r - e, t), pos2(r, b)),
            Dir::East,
            Cursor::ResizeEast,
        ),
        (
            Rect::from_min_max(pos2(l, t), pos2(l + c, t + c)),
            Dir::NorthWest,
            Cursor::ResizeNorthWest,
        ),
        (
            Rect::from_min_max(pos2(r - c, t), pos2(r, t + c)),
            Dir::NorthEast,
            Cursor::ResizeNorthEast,
        ),
        (
            Rect::from_min_max(pos2(l, b - c), pos2(l + c, b)),
            Dir::SouthWest,
            Cursor::ResizeSouthWest,
        ),
        (
            Rect::from_min_max(pos2(r - c, b - c), pos2(r, b)),
            Dir::SouthEast,
            Cursor::ResizeSouthEast,
        ),
    ]
}

/// A borderless window gets no OS resize frame, so the overlay supplies its
/// own: invisible strips along the edges that hand the gesture back to the
/// window manager via `BeginResize`.
fn draw_resize_handles(ui: &mut egui::Ui, ctx: &egui::Context) {
    let window = ctx.input(|i| i.viewport_rect());
    // `ResizeDirection` is not `Hash`, so the zone's position in the array is
    // what keeps the eight ids distinct.
    for (index, (zone, direction, cursor)) in resize_zones(window).into_iter().enumerate() {
        let handle = ui.interact(zone, ui.id().with(("resize", index)), egui::Sense::drag());
        if handle.hovered() {
            ctx.set_cursor_icon(cursor);
        }
        // Same as the title-bar drag: this opens a modal loop on the OS side,
        // so it must fire once per gesture, not once per frame.
        if handle.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

fn draw_rows(ui: &mut egui::Ui, snapshot: &Snapshot, columns: &[ColumnKind], icons: &ClassIcons) {
    // The enabled-column set (and therefore the column widths and their
    // anchors) is identical for every row in a frame, so both are computed
    // once here rather than once per row inside `draw_row`.
    let stat_columns = stat_columns_for(columns);
    let avail = ui.available_rect_before_wrap();
    let anchors = column_anchors(avail.left(), avail.right(), &stat_columns, 4.0);

    for row in &snapshot.rows {
        draw_row(ui, row, &stat_columns, &anchors, icons);
    }
}

/// One column in the stats row (issue #8). `width` is the fixed on-screen
/// space, in points, that this column reserves to its own *left*: per
/// `column_anchors`, the gap between the previous column's anchor and this
/// column's anchor equals this column's width (see
/// `column_anchors_spacing_matches_column_widths_when_room_allows`). A
/// digit-count change in the painted text therefore never shifts any
/// column's anchor point. `text` renders this column's value for a given
/// row, so a column's width and its formatter travel together.
#[derive(Debug, Clone, Copy)]
pub struct StatColumn {
    pub width: f32,
    pub text: fn(&PlayerRow) -> String,
}

/// Builds the column specs for `column_anchors` out of the currently
/// enabled `ColumnKind`s (issue #13: the column set is now dynamic and
/// user-filterable rather than the fixed-size array issue #8 shipped).
///
/// Each kind hands over both its width and its `text` formatter in one
/// `StatColumn` (`ColumnKind::spec`), and `column_anchors` produces exactly
/// one anchor per entry here, so `draw_row` can zip anchors against this
/// same slice: a column can never end up with an anchor but no text, and
/// adding a `ColumnKind` cannot skip wiring up its rendering.
fn stat_columns_for(columns: &[ColumnKind]) -> Vec<StatColumn> {
    columns.iter().map(|c| c.spec()).collect()
}

/// Computes each column's right-aligned text anchor (an x coordinate, for
/// use with `Align2::RIGHT_CENTER`), given the row rect's left/right edges,
/// the column specs (left-to-right), and the margin from the rect's right
/// edge to the rightmost column's anchor.
///
/// Anchors are placed right-to-left starting at `rect_right - margin`, each
/// preceding column offset left by the next column's width. This is a pure
/// function of the rect and column specs — it never sees the text that will
/// be painted — so a column's anchor is stable regardless of how many
/// digits its number has.
///
/// When the available width (`rect_right - rect_left - margin`) is less
/// than the columns' combined width, every column width is scaled down
/// proportionally so the columns still fit rather than spilling past the
/// rect's left edge — graceful degradation for a narrow window.
pub fn column_anchors(
    rect_left: f32,
    rect_right: f32,
    columns: &[StatColumn],
    margin: f32,
) -> Vec<f32> {
    let total_width: f32 = columns.iter().map(|c| c.width).sum();
    let available = (rect_right - rect_left - margin).max(0.0);
    let scale = if total_width > available && total_width > 0.0 {
        available / total_width
    } else {
        1.0
    };

    let mut anchors = Vec::with_capacity(columns.len());
    let mut x = rect_right - margin;
    for col in columns.iter().rev() {
        anchors.push(x);
        x -= col.width * scale;
    }
    anchors.reverse();
    anchors
}

fn draw_row(
    ui: &mut egui::Ui,
    row: &PlayerRow,
    columns: &[StatColumn],
    anchors: &[f32],
    icons: &ClassIcons,
) {
    let desired_size = egui::vec2(ui.available_width(), ROW_HEIGHT);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    // Proportional background bar scaled by this player's damage share.
    // Painted before (i.e. under) the icon and name, and still spans the
    // row's full width — the icon slot is reserved on top of it, not cut
    // out of it. Split into a subtle wash plus a crisp bottom underline
    // (issue #43) rather than one flat fill, matching the reference meter.
    let paints = share_bar_paints(rect, row.share_pct);
    ui.painter()
        .rect_filled(paints.wash_rect, 2.0, paints.wash_color);
    ui.painter()
        .rect_filled(paints.underline_rect, 0.0, paints.underline_color);

    // The icon slot (issue #9) is reserved at a fixed offset regardless of
    // whether this row's class has an icon, so names stay left-aligned in a
    // column across rows either way — only the painting below is
    // conditional.
    let (icon_rect, name_offset) = icon_slot(rect);
    if let Some(texture) = row.class.and_then(|class| icons.get(class)) {
        ui.painter().image(
            texture.id(),
            icon_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    let name = row_name(row);
    ui.painter().text(
        rect.left_center() + egui::vec2(name_offset, 0.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::monospace(13.0),
        egui::Color32::WHITE,
    );

    // Each stat gets its own fixed-width column (issue #8) so a
    // digit-count change (e.g. `9.9K` -> `10.1K`) shifts only that
    // column's text, never the column's anchor point. Which columns
    // appear, and in what order, comes from the user's settings (#13).
    // Widths and anchors are row-invariant within a frame, so the caller
    // (`draw_rows`) builds the column slice and its anchors once and hands
    // both down. Anchors and text still come from that one slice, so the
    // two can never drift apart in length (issue #8 review):
    // `column_anchors` yields exactly one anchor per `StatColumn`, and each
    // `StatColumn` carries its own formatter.
    for (anchor_x, column) in anchors.iter().zip(columns) {
        let text = (column.text)(row);
        ui.painter().text(
            egui::pos2(*anchor_x, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::monospace(13.0),
            egui::Color32::WHITE,
        );
    }
}

/// Base RGB of the damage-share bar (issue #43). Split out from the alpha
/// constants below so a follow-up issue (#44, role-based bar color) can vary
/// only the hue — the wash/underline alpha split and underline thickness
/// stay fixed regardless of which color a role ends up using.
const SHARE_BAR_RGB: (u8, u8, u8) = (60, 120, 220);

/// Alpha of the translucent wash covering the full width of `bar_rect`
/// (issue #43). Deliberately lower than the old single flat fill's alpha
/// (120) — the wash now only needs to read as a subtle backdrop, since the
/// underline below is what carries the crisp share boundary.
const SHARE_BAR_WASH_ALPHA: u8 = 60;

/// Alpha of the thin strip along `bar_rect`'s bottom edge (issue #43).
/// Markedly more opaque than the wash so the share boundary still reads
/// clearly at a glance even though the rest of the bar is subtle, matching
/// the reference meter (`docs/reference/tera_shinrameter_ex.png`).
const SHARE_BAR_UNDERLINE_ALPHA: u8 = 220;

/// Thickness of the bottom underline strip (issue #43). `share_bar_paints`
/// clamps this against the row height so it stays sane — never taller than
/// the row itself — at small row heights.
const SHARE_BAR_UNDERLINE_THICKNESS: f32 = 2.0;

/// The two paints that make up a row's damage-share bar (issue #43): a
/// translucent wash across the full share-scaled width, and a thin,
/// more-opaque underline strip along its bottom edge. Named fields rather
/// than a positional tuple so a wash/underline (or rect/color) mix-up at the
/// `draw_row` call site fails to compile instead of silently swapping which
/// paint lands where.
struct ShareBarPaints {
    wash_rect: egui::Rect,
    underline_rect: egui::Rect,
    wash_color: egui::Color32,
    underline_color: egui::Color32,
}

/// Computes the two paints that make up a row's damage-share bar (issue
/// #43): a translucent wash across the full share-scaled width, and a thin,
/// more-opaque underline strip along its bottom edge so the share boundary
/// reads crisply even though the wash itself is subtle. Pure geometry/color
/// math with no `egui::Ui` dependency, so it's unit-testable on its own —
/// `draw_row` just paints whatever it returns.
fn share_bar_paints(rect: egui::Rect, share_pct: f32) -> ShareBarPaints {
    let bar_frac = (share_pct / 100.0).clamp(0.0, 1.0);
    let bar_width = rect.width() * bar_frac;

    let wash_rect = egui::Rect::from_min_size(rect.min, egui::vec2(bar_width, rect.height()));

    let thickness = SHARE_BAR_UNDERLINE_THICKNESS.min(rect.height());
    let underline_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, rect.max.y - thickness),
        egui::vec2(bar_width, thickness),
    );

    let (r, g, b) = SHARE_BAR_RGB;
    let wash_color = egui::Color32::from_rgba_unmultiplied(r, g, b, SHARE_BAR_WASH_ALPHA);
    let underline_color = egui::Color32::from_rgba_unmultiplied(r, g, b, SHARE_BAR_UNDERLINE_ALPHA);

    ShareBarPaints {
        wash_rect,
        underline_rect,
        wash_color,
        underline_color,
    }
}

/// Square side of the per-row class icon (issue #9), roughly the row's text
/// height.
const ICON_SIZE: f32 = 16.0;

/// Gap on both sides of the icon: between the row's left edge and the icon,
/// and between the icon and the name that follows it.
const ICON_MARGIN: f32 = 3.0;

/// Fixed left-hand gutter `draw_row` reserves for the class icon slot: a
/// margin, the icon itself, then a matching margin — reserved whether or
/// not this particular row has an icon to paint into it, so every row's
/// name still starts at the same x (see `icon_slot`).
const ICON_GUTTER_WIDTH: f32 = ICON_MARGIN + ICON_SIZE + ICON_MARGIN;

/// Computes a row's icon slot (a square, vertically centered in `rect`,
/// inset from the left edge by `ICON_MARGIN`) and the x-offset from
/// `rect`'s left edge at which the player name should then start. Pure
/// geometry — it never looks at whether this row actually has a class icon
/// to paint — so the slot, and therefore the name's start position, is
/// identical across every row regardless of which classes have icons.
fn icon_slot(rect: egui::Rect) -> (egui::Rect, f32) {
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + ICON_MARGIN, rect.center().y - ICON_SIZE / 2.0),
        egui::vec2(ICON_SIZE, ICON_SIZE),
    );
    let name_offset = ICON_GUTTER_WIDTH + NAME_LEFT_PAD;
    (icon_rect, name_offset)
}

/// `bpsr_meter` already fills unknown names with `Player {uid}`; this is a
/// defensive fallback in case a row ever arrives with an empty name.
fn row_name(row: &PlayerRow) -> String {
    if row.name.is_empty() {
        format!("Player {}", row.uid)
    } else {
        row.name.clone()
    }
}

/// Compact damage abbreviation: `999`, `1.0K`, `1.2M`, `1.0B`.
pub fn fmt_short(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let av = v.unsigned_abs();

    if av >= 1_000_000_000 {
        format!("{sign}{:.1}B", av as f64 / 1_000_000_000.0)
    } else if av >= 1_000_000 {
        format!("{sign}{:.1}M", av as f64 / 1_000_000.0)
    } else if av >= 1_000 {
        format!("{sign}{:.1}K", av as f64 / 1_000.0)
    } else {
        format!("{sign}{av}")
    }
}

/// Fight duration as `m:ss`.
pub fn fmt_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

/// Damage-share percentage as `12.3%`.
pub fn fmt_share(share_pct: f32) -> String {
    format!("{share_pct:.1}%")
}

// -- default window size (issue #26) -----------------------------------
//
// The old default, `[340.0, 220.0]`, was sized for ~8-9 rows and left no
// room for names next to the default stat columns. These constants and
// helpers derive a default that fits a full 20-player raid with no
// scrolling, and leaves a name a real budget before the stat columns
// start, out of the same numbers `draw_header`/`draw_row`/`apply_theme`
// already use — so a future change to any of them (e.g. a taller row)
// updates the default size instead of silently drifting out of sync with it.

/// Number of player rows the default height fits without scrolling: a full
/// raid roster.
const DEFAULT_VISIBLE_ROWS: usize = 20;

/// `draw_row`'s fixed row height (its `desired_size.y`), named here so the
/// default-size math and the row painter can never drift apart.
const ROW_HEIGHT: f32 = 20.0;

/// egui's fixed height for `ui.separator()`'s own painted line
/// (`Style::separator_style`'s `spacing: 6.0`) — a constant of egui's, not
/// anything `apply_theme` overrides.
const SEPARATOR_HEIGHT: f32 = 6.0;

/// Vertical gap egui's layout inserts between consecutive widgets
/// (`apply_theme` sets `style.spacing.item_spacing` to `(6.0, 2.0)`). The
/// header band, the separator, and each of the 20 rows are each their own
/// widget in the central panel's vertical layout, so this gap is paid once
/// between every consecutive pair of them.
const ITEM_SPACING_Y: f32 = 2.0;

/// Gap `draw_row` leaves between the icon slot (issue #9's `ICON_GUTTER_WIDTH`)
/// and where the player name starts (`icon_slot`'s `name_offset`). Predates
/// the icon slot — this used to be measured from the row's own left edge —
/// but keeps its name since it's still the same "breathing room before the
/// name" budget.
const NAME_LEFT_PAD: f32 = 4.0;

/// Budgeted width for the name itself. `draw_row` paints names unclipped in
/// `FontId::monospace(13.0)` — truncation/ellipsis is explicitly out of
/// scope for issue #26 — so this is not a hard cap, just enough room
/// (roughly 15 monospace characters at this font size) that a typical
/// alphanumeric in-game name doesn't visually crowd the stat columns that
/// start right after it.
const NAME_WIDTH_BUDGET: f32 = 150.0;

/// Breathing room between the name budget and the first stat column.
const NAME_COLUMN_GAP: f32 = 12.0;

/// Right-edge margin, matching the `margin` `draw_rows` passes to
/// `column_anchors` for the rightmost column's anchor.
const COLUMN_RIGHT_MARGIN: f32 = 4.0;

/// Default opening height (issue #26; extended by issue #9 slice 2's title
/// line): the header's title line + timer/DPS/buttons row + separator + a
/// full 20-row raid roster, plus the `ITEM_SPACING_Y` gap egui's layout
/// inserts between each of those 23 widgets (22 gaps), so no scrolling is
/// needed on first launch. The subtitle line is deliberately excluded — it
/// is conditional (only rendered once a scene name/id is known,
/// `encounter_subtitle`), and the default assumes it is absent.
///
///   title (20.0) + header row (18.0) + separator (6.0) + 20 rows * 20.0 (400.0)
///     + 22 gaps * 2.0 (44.0) = 488.0
fn default_inner_height() -> f32 {
    let header_row = egui::Style::default().spacing.interact_size.y;
    let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
    let gaps = (DEFAULT_VISIBLE_ROWS + 2) as f32 * ITEM_SPACING_Y;
    TITLE_LINE_HEIGHT + header_row + SEPARATOR_HEIGHT + rows + gaps
}

/// Default opening width (issue #26, widened for issue #9's icon gutter): a
/// name budget in front of the default stat columns' combined fixed width
/// (`Settings::default`'s Damage + DPS + Share % = 188pt total, from
/// `ColumnKind::spec` in `settings.rs`), so names don't visually collide
/// with them — plus the fixed icon gutter now reserved at the row's left
/// edge, so adding it doesn't squeeze the name budget or the stat columns
/// relative to before issue #9.
///
///   icon gutter (3.0 + 16.0 + 3.0 = 22.0) + left pad (4.0)
///     + name budget (150.0) + gap (12.0)
///     + columns (56.0 + 76.0 + 56.0 = 188.0) + right margin (4.0) = 380.0
fn default_inner_width() -> f32 {
    let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
        .iter()
        .map(|c| c.width)
        .sum();
    ICON_GUTTER_WIDTH
        + NAME_LEFT_PAD
        + NAME_WIDTH_BUDGET
        + NAME_COLUMN_GAP
        + columns_width
        + COLUMN_RIGHT_MARGIN
}

/// Overlay window shape: always-on-top, borderless, transparent, sized to
/// fit a full raid by default (issue #26). `window_position` is the
/// last-saved position (issue #27, `Settings::window_position`) to reopen
/// at, or `None` on a first launch / wiped settings file, which leaves the
/// position to today's default OS/winit placement.
pub fn viewport(window_position: Option<[f32; 2]>) -> egui::ViewportBuilder {
    let mut builder = egui::ViewportBuilder::default()
        .with_always_on_top()
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_inner_size([default_inner_width(), default_inner_height()])
        .with_min_inner_size([220.0, 90.0]);
    if let Some(position) = window_position {
        builder = builder.with_position(position);
    }
    builder
}

/// Dark, compact visuals with monospace numerals for the overlay.
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgba_unmultiplied(18, 18, 22, 200);
    visuals.window_fill = visuals.panel_fill;
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 2.0);
        style.spacing.button_padding = egui::vec2(4.0, 2.0);
        // Labels sense click-and-drag when selectable, which would swallow the
        // header drag as a text selection. Nothing here is worth selecting.
        style.interaction.selectable_labels = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ColumnKind, Settings};
    use bpsr_meter::Class;

    #[test]
    fn fmt_short_below_thousand_is_plain() {
        assert_eq!(fmt_short(999), "999");
    }

    #[test]
    fn fmt_short_thousands() {
        assert_eq!(fmt_short(1_000), "1.0K");
    }

    #[test]
    fn fmt_short_millions() {
        assert_eq!(fmt_short(1_234_567), "1.2M");
    }

    #[test]
    fn fmt_short_negative() {
        assert_eq!(fmt_short(-1_500), "-1.5K");
    }

    #[test]
    fn fmt_short_billions() {
        assert_eq!(fmt_short(2_500_000_000), "2.5B");
    }

    #[test]
    fn fmt_duration_zero() {
        assert_eq!(fmt_duration(0), "0:00");
    }

    #[test]
    fn fmt_duration_minute_and_seconds() {
        assert_eq!(fmt_duration(65_000), "1:05");
    }

    #[test]
    fn fmt_duration_no_hour_rollover() {
        assert_eq!(fmt_duration(3_600_000), "60:00");
    }

    #[test]
    fn fmt_share_zero() {
        assert_eq!(fmt_share(0.0), "0.0%");
    }

    #[test]
    fn fmt_share_rounds_to_one_decimal() {
        assert_eq!(fmt_share(12.34), "12.3%");
    }

    #[test]
    fn fmt_share_full() {
        assert_eq!(fmt_share(100.0), "100.0%");
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
    fn title_shows_monster_id_for_unnamed_boss() {
        // The two vendored lists aren't guaranteed to agree (see
        // `EncounterInfo::is_boss`'s doc comment) — a recognized boss can
        // still have no resolved name. That's a real boss fight, so it must
        // not render as blank (indistinguishable from a trash pull); it
        // falls back to the same `Monster #{id}` style the header used
        // before issue #42 gated it to boss fights only.
        let e = EncounterInfo {
            boss_monster_id: Some(999_999),
            boss_name: None,
            is_boss: true,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Monster #999999");
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

    fn sample_row(ability_score: Option<u32>) -> PlayerRow {
        PlayerRow {
            uid: 1,
            name: String::new(),
            class: None,
            damage: 0,
            dps: 0.0,
            share_pct: 0.0,
            crit_pct: 0.0,
            lucky_pct: 0.0,
            hits: 0,
            ability_score,
            season_level: None,
            season_strength: None,
        }
    }

    fn sample_season_row(season_level: Option<u32>, season_strength: Option<u32>) -> PlayerRow {
        PlayerRow {
            season_level,
            season_strength,
            ..sample_row(None)
        }
    }

    #[test]
    fn ability_score_column_blank_when_none() {
        let row = sample_row(None);
        let column = ColumnKind::AbilityScore.spec();
        assert_eq!((column.text)(&row), "");
    }

    #[test]
    fn ability_score_column_formats_value_when_some() {
        let row = sample_row(Some(12_345));
        let column = ColumnKind::AbilityScore.spec();
        assert_eq!((column.text)(&row), fmt_short(12_345));
    }

    #[test]
    fn season_level_column_blank_when_none() {
        let row = sample_season_row(None, None);
        let column = ColumnKind::SeasonLevel.spec();
        assert_eq!((column.text)(&row), "");
    }

    #[test]
    fn season_level_column_formats_value_when_some() {
        let row = sample_season_row(Some(42), None);
        let column = ColumnKind::SeasonLevel.spec();
        assert_eq!((column.text)(&row), fmt_short(42));
    }

    #[test]
    fn season_strength_column_blank_when_none() {
        let row = sample_season_row(None, None);
        let column = ColumnKind::SeasonStrength.spec();
        assert_eq!((column.text)(&row), "");
    }

    #[test]
    fn season_strength_column_formats_value_when_some() {
        let row = sample_season_row(None, Some(12_345));
        let column = ColumnKind::SeasonStrength.spec();
        assert_eq!((column.text)(&row), fmt_short(12_345));
    }

    fn window() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(340.0, 220.0))
    }

    #[test]
    fn resize_zones_cover_every_direction() {
        let dirs: Vec<_> = resize_zones(window())
            .map(|(_, d, _)| d)
            .into_iter()
            .collect();
        assert_eq!(dirs.len(), 8);
        for dir in [
            egui::ResizeDirection::North,
            egui::ResizeDirection::South,
            egui::ResizeDirection::East,
            egui::ResizeDirection::West,
            egui::ResizeDirection::NorthEast,
            egui::ResizeDirection::SouthEast,
            egui::ResizeDirection::NorthWest,
            egui::ResizeDirection::SouthWest,
        ] {
            assert!(dirs.contains(&dir), "missing {dir:?}");
        }
    }

    #[test]
    fn resize_zones_stay_inside_the_window() {
        let win = window();
        for (zone, dir, _) in resize_zones(win) {
            assert!(win.contains_rect(zone), "{dir:?} escapes the window");
        }
    }

    /// Corners must be registered after the edges they overlap, so the later
    /// widget wins the hit test and a corner grab resizes both axes.
    #[test]
    fn corners_are_registered_after_edges() {
        let zones = resize_zones(window());
        let first_corner = zones
            .iter()
            .position(|(_, d, _)| {
                matches!(
                    d,
                    egui::ResizeDirection::NorthWest
                        | egui::ResizeDirection::NorthEast
                        | egui::ResizeDirection::SouthWest
                        | egui::ResizeDirection::SouthEast
                )
            })
            .expect("a corner zone exists");
        let last_edge = zones
            .iter()
            .rposition(|(_, d, _)| {
                matches!(
                    d,
                    egui::ResizeDirection::North
                        | egui::ResizeDirection::South
                        | egui::ResizeDirection::East
                        | egui::ResizeDirection::West
                )
            })
            .expect("an edge zone exists");
        assert!(last_edge < first_corner);
    }

    /// The header drag band must clear the north strip, or the title bar eats
    /// every top-edge resize.
    #[test]
    fn north_strip_is_not_swallowed_by_the_header() {
        let win = window();
        let (north, ..) = resize_zones(win)[0];
        assert_eq!(north.height(), RESIZE_EDGE);
        assert_eq!(north.top(), win.top());
    }

    // -- header_band_height (drag band must cover the rendered header) ----

    /// No subtitle: egui stacks title + button row, one gap between them.
    #[test]
    fn header_band_height_with_no_subtitle_covers_title_gap_and_button_row() {
        let button_row_height = 18.0;
        let expected = TITLE_LINE_HEIGHT + button_row_height + ITEM_SPACING_Y;
        assert_eq!(header_band_height(false, button_row_height), expected);
    }

    /// With a subtitle: egui stacks title + subtitle + button row, so there
    /// are two gaps, not one — the bug this guards against undercounted by
    /// exactly one `ITEM_SPACING_Y` here.
    #[test]
    fn header_band_height_with_subtitle_covers_both_gaps() {
        let button_row_height = 18.0;
        let expected =
            TITLE_LINE_HEIGHT + SUBTITLE_LINE_HEIGHT + button_row_height + 2.0 * ITEM_SPACING_Y;
        assert_eq!(header_band_height(true, button_row_height), expected);
    }

    /// Adding the subtitle must grow the band by exactly the subtitle's own
    /// height plus the extra gap it introduces — not by a smaller amount
    /// (the original bug: gaps were never added, so a subtitle only grew the
    /// band by `SUBTITLE_LINE_HEIGHT`, leaving the band 4px short).
    #[test]
    fn subtitle_grows_band_by_its_height_plus_one_extra_gap() {
        let button_row_height = 18.0;
        let without = header_band_height(false, button_row_height);
        let with = header_band_height(true, button_row_height);
        assert_eq!(with - without, SUBTITLE_LINE_HEIGHT + ITEM_SPACING_Y);
    }

    // -- column_anchors (issue #8) --------------------------------------

    /// A stand-in three-column layout (same widths the old fixed
    /// `STAT_COLUMNS` array used) for tests that exercise `column_anchors`'
    /// pure math and don't care where the widths came from.
    const TEST_COLUMNS: [StatColumn; 3] = [
        StatColumn {
            width: 56.0,
            text: |row| fmt_short(row.damage),
        },
        StatColumn {
            width: 56.0,
            text: |row| format!("{}/s", fmt_short(row.dps as i64)),
        },
        StatColumn {
            width: 44.0,
            text: |row| fmt_share(row.share_pct),
        },
    ];

    /// The anchor for a column never depends on what text will be painted
    /// into it — only on the row rect and the column specs — so a
    /// digit-count change in the text can never shift it. Calling the
    /// function twice with identical inputs (standing in for "row before"
    /// vs. "row after" a digit-count change) must yield identical anchors.
    #[test]
    fn column_anchors_are_stable_across_repeated_calls() {
        let first = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        let second = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        assert_eq!(first, second);
    }

    #[test]
    fn column_anchors_rightmost_column_sits_margin_from_right_edge() {
        let anchors = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        let last = *anchors.last().unwrap();
        assert_eq!(last, 300.0 - 4.0);
    }

    #[test]
    fn column_anchors_are_ordered_left_to_right() {
        let anchors = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        for pair in anchors.windows(2) {
            assert!(pair[0] < pair[1], "anchors must increase left-to-right");
        }
    }

    #[test]
    fn column_anchors_spacing_matches_column_widths_when_room_allows() {
        let anchors = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        // Ample room: no scaling, so consecutive anchors are exactly one
        // column-width apart (the earlier column's own width).
        for i in 0..TEST_COLUMNS.len() - 1 {
            let gap = anchors[i + 1] - anchors[i];
            assert_eq!(gap, TEST_COLUMNS[i + 1].width);
        }
    }

    #[test]
    fn column_anchors_degrade_gracefully_in_a_narrow_window() {
        // Total column width plus margin exceeds the available rect, so the
        // columns must shrink to fit rather than spilling past the left
        // edge indefinitely.
        let total: f32 = TEST_COLUMNS.iter().map(|c| c.width).sum();
        let narrow_right = total * 0.5;
        let anchors = column_anchors(0.0, narrow_right, &TEST_COLUMNS, 4.0);
        assert!(*anchors.first().unwrap() >= 0.0);
        // Still ordered and still anchored to the right edge.
        for pair in anchors.windows(2) {
            assert!(pair[0] < pair[1]);
        }
        assert_eq!(*anchors.last().unwrap(), narrow_right - 4.0);
    }

    #[test]
    fn column_anchors_unaffected_by_rect_left_when_room_allows() {
        // Anchors are computed from the right edge; moving the rect's left
        // edge around (without shrinking available width below the column
        // total) must not move them.
        let a = column_anchors(0.0, 300.0, &TEST_COLUMNS, 4.0);
        let b = column_anchors(50.0, 300.0, &TEST_COLUMNS, 4.0);
        assert_eq!(a, b);
    }

    /// The geometry tests above only check anchor placement; none of them
    /// confirm the *painted text* actually fits inside its column's width
    /// budget. This builds the widest plausible row, renders every
    /// selectable column's text through its own `StatColumn::text` with the
    /// same font `draw_row` paints with (`FontId::monospace(13.0)`), and
    /// asserts each fits inside that column's `width`. Running over
    /// `ColumnKind::ALL` holds every column — including any added later —
    /// to its own budget. Pre-fix, the dps column reused the damage
    /// column's 56.0-wide budget even though its text carries a 2-char
    /// "/s" suffix on top of `fmt_short`'s ~7-char max — this test fails
    /// against that width.
    #[test]
    fn widest_formatted_text_fits_its_column_width_budget() {
        let ctx = egui::Context::default();
        // Load the real (non-empty) default fonts, so glyph metrics match
        // what `draw_row` actually paints with, then discard the resulting
        // font-atlas texture upload — nothing is painted in this test.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        // Widest plausible value for every field any column formats:
        // `fmt_short`'s 7-char maximum (rounds up across a K/M/B
        // threshold, e.g. 999_950 -> "1000.0K") and `fmt_share`'s.
        assert_eq!(fmt_short(999_950), "1000.0K");
        assert_eq!(fmt_share(100.0), "100.0%");
        let widest_row = PlayerRow {
            uid: 1,
            name: String::new(),
            class: None,
            damage: 999_950,
            dps: 999_950.0,
            share_pct: 100.0,
            crit_pct: 100.0,
            lucky_pct: 100.0,
            hits: 999_950,
            ability_score: Some(999_950),
            season_level: Some(999_950),
            season_strength: Some(999_950),
        };

        for (kind, column) in ColumnKind::ALL
            .into_iter()
            .zip(stat_columns_for(&ColumnKind::ALL))
        {
            let text = (column.text)(&widest_row);
            let text_width = ctx.fonts_mut(|f| {
                f.layout_no_wrap(
                    text.clone(),
                    egui::FontId::monospace(13.0),
                    egui::Color32::WHITE,
                )
                .rect
                .width()
            });
            assert!(
                text_width <= column.width,
                "{kind:?}: {text:?} is {text_width}pt wide, wider than its {}pt column budget",
                column.width
            );
        }
    }

    // -- icon slot geometry (issue #9) ------------------------------------

    fn row_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(300.0, ROW_HEIGHT))
    }

    #[test]
    fn icon_slot_is_square() {
        let (icon_rect, _) = icon_slot(row_rect());
        assert_eq!(icon_rect.width(), ICON_SIZE);
        assert_eq!(icon_rect.height(), ICON_SIZE);
    }

    #[test]
    fn icon_slot_is_inset_from_the_rows_left_edge_by_the_margin() {
        let rect = row_rect();
        let (icon_rect, _) = icon_slot(rect);
        assert_eq!(icon_rect.left(), rect.left() + ICON_MARGIN);
    }

    #[test]
    fn icon_slot_is_vertically_centered_in_the_row() {
        let rect = row_rect();
        let (icon_rect, _) = icon_slot(rect);
        assert_eq!(icon_rect.center().y, rect.center().y);
    }

    #[test]
    fn icon_slot_name_offset_clears_the_icon_with_its_own_margin() {
        let (icon_rect, name_offset) = icon_slot(row_rect());
        let rect = row_rect();
        // The name must start at or after the icon's right edge plus its own
        // margin gap — never overlapping the icon.
        assert!(rect.left() + name_offset >= icon_rect.right() + ICON_MARGIN);
    }

    #[test]
    fn icon_slot_name_offset_equals_the_gutter_plus_name_pad() {
        let (_, name_offset) = icon_slot(row_rect());
        assert_eq!(name_offset, ICON_GUTTER_WIDTH + NAME_LEFT_PAD);
    }

    /// The slot is reserved unconditionally: its geometry depends only on
    /// the row rect, never on anything row-specific like whether this
    /// player's class actually has an icon — so identical rects must always
    /// yield identical slots.
    #[test]
    fn icon_slot_is_independent_of_row_width() {
        let narrow =
            egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(50.0, ROW_HEIGHT));
        let wide =
            egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(500.0, ROW_HEIGHT));
        assert_eq!(icon_slot(narrow), icon_slot(wide));
    }

    // -- damage-share bar paints (issue #43) --------------------------------

    fn share_bar_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(300.0, ROW_HEIGHT))
    }

    #[test]
    fn share_bar_full_share_spans_the_full_width() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 100.0);
        assert_eq!(paints.wash_rect.width(), rect.width());
        assert_eq!(paints.underline_rect.width(), rect.width());
    }

    #[test]
    fn share_bar_zero_share_has_no_width() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 0.0);
        assert_eq!(paints.wash_rect.width(), 0.0);
        assert_eq!(paints.underline_rect.width(), 0.0);
    }

    #[test]
    fn share_bar_partial_share_scales_both_rects_identically() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 40.0);
        assert_eq!(paints.wash_rect.width(), rect.width() * 0.4);
        assert_eq!(paints.underline_rect.width(), rect.width() * 0.4);
    }

    /// The underline is what makes the share boundary read crisply (issue
    /// #43), so it must hug `rect`'s bottom edge rather than float somewhere
    /// inside the bar.
    #[test]
    fn share_bar_underline_sits_at_the_bottom_edge() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 50.0);
        assert_eq!(paints.underline_rect.bottom(), rect.bottom());
        assert_eq!(
            paints.underline_rect.height(),
            SHARE_BAR_UNDERLINE_THICKNESS
        );
    }

    /// A row short enough that the fixed underline thickness would exceed
    /// its height must clamp the underline down to the row height instead
    /// of spilling past the row's top edge.
    #[test]
    fn share_bar_underline_thickness_clamps_at_a_tiny_row_height() {
        let tiny_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 1.0));
        let paints = share_bar_paints(tiny_rect, 50.0);
        assert!(paints.underline_rect.height() <= tiny_rect.height());
        assert_eq!(paints.underline_rect.top(), tiny_rect.top());
    }

    /// The wash must stay markedly more translucent than the underline
    /// (issue #43) — that alpha gap is what lets the underline carry the
    /// crisp share boundary while the wash reads as a subtle backdrop.
    #[test]
    fn share_bar_wash_alpha_is_lower_than_underline_alpha() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 50.0);
        assert!(paints.wash_color.a() < paints.underline_color.a());
    }

    // -- class -> asset mapping totality (issue #9) ------------------------
    //
    // `ClassIcons` (in `crate::icons`) has its own totality test over
    // `CLASS_ICON_BYTES`; this one checks the same property from the
    // rendering side: every non-`Unknown` `Class` a row can carry resolves
    // to *some* icon lookup outcome without panicking, and `Unknown`
    // resolves to none.
    #[test]
    fn class_icons_get_is_defined_for_every_class_including_unknown() {
        let ctx = egui::Context::default();
        let icons = ClassIcons::load(&ctx);

        for class in [
            Class::Stormblade,
            Class::FrostMage,
            Class::TwinStriker,
            Class::WindKnight,
            Class::VerdantOracle,
            Class::HeavyGuardian,
            Class::Marksman,
            Class::ShieldKnight,
            Class::BeatPerformer,
        ] {
            assert!(icons.get(class).is_some(), "{class:?} has no icon");
        }
        assert!(icons.get(Class::Unknown).is_none());
    }

    // -- dynamic columns from settings (issue #13) -----------------------
    //
    // `stat_columns_for` (production, above) builds column specs from the
    // enabled `ColumnKind`s; these tests exercise that it still preserves
    // `column_anchors`' invariants as the enabled set changes.

    #[test]
    fn default_settings_columns_match_original_three_column_layout() {
        let cols = Settings::default().ordered_columns();
        let anchors = column_anchors(0.0, 300.0, &stat_columns_for(&cols), 4.0);

        // Same three columns (damage/dps/share%) as the original fixed
        // STAT_COLUMNS array from issue #8.
        assert_eq!(anchors.len(), 3);
        assert_eq!(*anchors.last().unwrap(), 300.0 - 4.0);
    }

    #[test]
    fn rightmost_anchor_stays_pinned_to_margin_as_columns_are_enabled() {
        let mut settings = Settings::default();
        let before = column_anchors(
            0.0,
            300.0,
            &stat_columns_for(&settings.ordered_columns()),
            4.0,
        );

        settings.toggle(ColumnKind::CritPct);
        settings.toggle(ColumnKind::Hits);
        let after = column_anchors(
            0.0,
            300.0,
            &stat_columns_for(&settings.ordered_columns()),
            4.0,
        );

        assert_eq!(*before.last().unwrap(), 300.0 - 4.0);
        assert_eq!(*after.last().unwrap(), 300.0 - 4.0);
        assert!(after.len() > before.len());
    }

    #[test]
    fn disabling_a_column_still_leaves_anchors_ordered_and_pinned() {
        let mut settings = Settings {
            visible_columns: ColumnKind::ALL.to_vec(),
            window_position: None,
        };
        settings.toggle(ColumnKind::Dps);
        let cols = settings.ordered_columns();
        let anchors = column_anchors(0.0, 300.0, &stat_columns_for(&cols), 4.0);

        assert_eq!(anchors.len(), ColumnKind::ALL.len() - 1);
        assert_eq!(*anchors.last().unwrap(), 300.0 - 4.0);
        for pair in anchors.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    // -- default window size (issue #26) ----------------------------------

    #[test]
    fn default_inner_height_fits_twenty_rows_without_scrolling() {
        // The row-content budget alone (20 rows * 20pt) must fit inside the
        // computed default height, with room left over for the header band
        // and separator on top of it.
        let rows_only = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        assert!(
            default_inner_height() > rows_only,
            "default height {} must exceed the {} rows themselves",
            default_inner_height(),
            rows_only
        );
    }

    #[test]
    fn default_inner_height_matches_title_plus_header_plus_separator_plus_rows_plus_gaps() {
        let header_row = egui::Style::default().spacing.interact_size.y;
        let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        let gaps = (DEFAULT_VISIBLE_ROWS + 2) as f32 * ITEM_SPACING_Y;
        let expected = TITLE_LINE_HEIGHT + header_row + SEPARATOR_HEIGHT + rows + gaps;
        assert_eq!(default_inner_height(), expected);
    }

    #[test]
    fn default_inner_width_matches_icon_gutter_plus_name_budget_plus_columns() {
        let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
            .iter()
            .map(|c| c.width)
            .sum();
        let expected = ICON_GUTTER_WIDTH
            + NAME_LEFT_PAD
            + NAME_WIDTH_BUDGET
            + NAME_COLUMN_GAP
            + columns_width
            + COLUMN_RIGHT_MARGIN;
        assert_eq!(default_inner_width(), expected);
    }

    #[test]
    fn default_inner_width_exceeds_the_default_stat_columns_width() {
        let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
            .iter()
            .map(|c| c.width)
            .sum();
        assert!(
            default_inner_width() > columns_width,
            "default width must leave room for a name in front of the {}pt of stat columns",
            columns_width
        );
    }

    #[test]
    fn default_size_stays_above_the_min_inner_size() {
        // `with_min_inner_size` is [220.0, 90.0] (unaffected by issue #26);
        // the default opening size must never start below its own floor.
        assert!(default_inner_width() >= 220.0);
        assert!(default_inner_height() >= 90.0);
    }

    // -- window position tracking (issue #27) -----------------------------

    /// Runs one frame with `outer_rect`/`minimized` reported for the
    /// viewport, calling `track_window_position` from inside it exactly like
    /// `OverlayApp::update` does. Returns everything it sent on the
    /// settings-writer channel.
    fn track_one_frame(
        settings: &mut Settings,
        outer_rect: Option<egui::Rect>,
        minimized: Option<bool>,
    ) -> Vec<Settings> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            input.viewport_id,
            egui::ViewportInfo {
                outer_rect,
                minimized,
                ..Default::default()
            },
        );

        let ctx = egui::Context::default();
        ctx.run_ui(input, |ui| track_window_position(ui.ctx(), settings, &tx))
            .drop_without_applying_deltas();

        drop(tx);
        rx.try_iter().collect()
    }

    fn outer_rect_at(x: f32, y: f32) -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(
            egui::pos2(x, y),
            egui::vec2(340.0, 220.0),
        ))
    }

    #[test]
    fn track_window_position_persists_a_moved_window() {
        let mut settings = Settings::default();

        let sent = track_one_frame(&mut settings, outer_rect_at(100.0, 200.0), Some(false));

        assert_eq!(settings.window_position, Some([100.0, 200.0]));
        assert_eq!(sent.len(), 1, "one move, one send");
        assert_eq!(sent[0].window_position, Some([100.0, 200.0]));
    }

    #[test]
    fn track_window_position_stays_quiet_when_the_window_has_not_moved() {
        let mut settings = Settings {
            window_position: Some([100.0, 200.0]),
            ..Settings::default()
        };

        let sent = track_one_frame(&mut settings, outer_rect_at(100.0, 200.0), Some(false));

        assert!(sent.is_empty(), "an unmoved window must not send");
        assert_eq!(settings.window_position, Some([100.0, 200.0]));
    }

    /// Minimizing parks the window somewhere the user never put it, so
    /// nothing reported while minimized is persisted — not even a position
    /// that looks perfectly ordinary.
    #[test]
    fn track_window_position_ignores_a_minimized_window() {
        let mut settings = Settings {
            window_position: Some([100.0, 200.0]),
            ..Settings::default()
        };

        let sent = track_one_frame(&mut settings, outer_rect_at(400.0, 500.0), Some(true));

        assert!(sent.is_empty(), "a minimized window must not send");
        assert_eq!(settings.window_position, Some([100.0, 200.0]));
    }

    /// Same parking spot, but reported before the `minimized` flag catches
    /// up — the plausibility floor is what rejects it.
    #[test]
    fn track_window_position_ignores_an_absurd_off_screen_position() {
        let mut settings = Settings {
            window_position: Some([100.0, 200.0]),
            ..Settings::default()
        };

        let sent = track_one_frame(&mut settings, outer_rect_at(-32000.0, -32000.0), None);

        assert!(sent.is_empty(), "a bogus position must not send");
        assert_eq!(settings.window_position, Some([100.0, 200.0]));
    }

    // -- toolbar icon buttons (issue #41) ----------------------------------

    /// `icon_button`'s and `minimize_button`'s footprint must land exactly
    /// on `interact_size.y` — the height `header_band_height` already
    /// budgets the button row at — or the header band and the actually
    /// rendered row would drift apart (the same class of bug the
    /// `header_band_height_*` tests above guard against for the subtitle).
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

    /// `icon_button` must fall back to the original glyph, not paint
    /// nothing or panic, when the texture failed to decode (belt-and-braces,
    /// mirrors `ClassIcons::get`'s `None` case).
    #[test]
    fn icon_button_falls_back_to_glyph_when_texture_is_none() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = icon_button(ui, None, "×", "Close");
            // A real widget was allocated — a non-zero rect — not a no-op.
            assert!(response.rect.width() > 0.0);
            assert!(response.rect.height() > 0.0);
        })
        .drop_without_applying_deltas();
    }

    /// `minimize_button`'s allocated rect must match `icon_button`'s
    /// footprint exactly (`TOOLBAR_ICON_SIZE` plus `button_padding` on all
    /// sides), so it doesn't stand out as an oddly-sized button in the row.
    #[test]
    fn minimize_button_footprint_matches_icon_button_size() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = minimize_button(ui);
            let padding = ui.spacing().button_padding;
            let expected = TOOLBAR_ICON_SIZE + 2.0 * padding.y;
            assert_eq!(response.rect.height(), expected);
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn toggle_order_does_not_affect_resulting_anchor_layout() {
        let mut a = Settings::default();
        a.toggle(ColumnKind::Hits);
        a.toggle(ColumnKind::CritPct);

        let mut b = Settings::default();
        b.toggle(ColumnKind::CritPct);
        b.toggle(ColumnKind::Hits);

        let anchors_a = column_anchors(0.0, 300.0, &stat_columns_for(&a.ordered_columns()), 4.0);
        let anchors_b = column_anchors(0.0, 300.0, &stat_columns_for(&b.ordered_columns()), 4.0);
        assert_eq!(anchors_a, anchors_b);
    }
}
