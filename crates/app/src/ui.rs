//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in.

use std::time::Duration;

use bpsr_meter::{Class, EncounterInfo, PlayerRow, Role, Snapshot};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::fonts;
use crate::icons::{ClassIcons, GlyphIcon, GlyphIcons, ToolbarIcon, ToolbarIcons};
use crate::settings::{ColumnKind, Settings};

// -- typography scale (issue #56, issue #62) ----------------------------
//
// Every text paint in this module goes through `regular`/`bold` plus one of
// the sizes below — no ad-hoc `egui::FontId` at a call site — so the scale
// lives in exactly one place and can be re-tuned as a whole.
//
// These sizes come from `mvvm_refactor_wip`'s XAML, not from eyeballing the
// render: the source's `MetricTextBlockStyle` is a flat `FontSize="13"` for
// the row name and every metric column, with no `FontWeight`, so the row
// scale is deliberately flat. Row hierarchy is carried by *color*, not size
// or weight — see `ColumnKind::spec`'s `STAT_TEXT_RGB`/`CRIT_PCT_RGB`/etc.
//
// There is no `TextStyle`/`Style::text_styles` override anywhere in this
// app: egui's defaults only cover its own widgets (the settings menu), and
// everything else here is painted through `egui::Painter` with an explicit
// `FontId`, so a style table would be a second, silently-diverging source of
// truth rather than a shared one.

/// Boss/encounter title — the source's `FontSize="13" FontWeight="DemiBold"`.
const FONT_SIZE_TITLE: f32 = 13.0;
/// The value inside the timer half-pill — the source's `FontSize="16"
/// FontWeight="DemiBold"`, the largest text in the UI.
const FONT_SIZE_TIMER: f32 = 16.0;
/// Every text in a player row — name and every metric column alike. The
/// source's `MetricTextBlockStyle` is a flat `FontSize="13"` with no
/// `FontWeight`: its rows carry their hierarchy in *color* (white DPS,
/// `#aaa` plain stats, LightCoral crit) rather than in size or weight, so
/// this replaces the four separate row sizes issue #56 approximated off the
/// render.
const FONT_SIZE_ROW: f32 = 13.0;
/// The value inside a header stat pill — `GeneralStatTextStyle`'s default.
const FONT_SIZE_PILL_VALUE: f32 = 12.0;
/// Dungeon/scene subtitle — the source's `FontSize="10"`.
const FONT_SIZE_SUBTITLE: f32 = 10.0;
/// The death counter inside its pill — `DeathsDT` is `MetricTextBlockStyle`
/// like every other column, so it is the row metric, not a smaller size.
const FONT_SIZE_COUNTER: f32 = FONT_SIZE_ROW;

/// Horizontal offset (points) a bold-intended text is repainted at, on top
/// of the first paint, to fake a heavier weight. Only used when no real bold
/// font was installed (`fonts::has_real_bold`) — i.e. always on Linux, where
/// this is developed and CI-tested, and on any Windows box whose font
/// directory somehow has neither Segoe UI nor Tahoma nor Arial bold. See
/// `paint_text`.
const FAUX_BOLD_OFFSET: f32 = 0.6;

/// Proportional text at `size`, the app's regular weight (issue #56:
/// `fonts.rs` puts a system humanist sans at the head of this family when it
/// can find one, with egui's bundled default behind it).
fn regular(size: f32) -> egui::FontId {
    egui::FontId::proportional(size)
}

/// Bold text at `size` — the named `"bold"` family `fonts.rs` registers,
/// but *only* when a real bold file was actually found. Without one this
/// hands back the plain proportional font and the extra `FAUX_BOLD_OFFSET`
/// pass in `paint_text` does the work instead.
///
/// The `has_real_bold` guard is not just cosmetic: a bare `egui::Context`
/// (every unit test in this module, since none of them install fonts) has no
/// `"bold"` family at all, and epaint *panics* when asked to lay text out in
/// a named family that is not bound to any font.
fn bold(size: f32) -> egui::FontId {
    if fonts::has_real_bold() {
        egui::FontId::new(size, fonts::bold_family())
    } else {
        regular(size)
    }
}

/// Paints `text` and returns its rect, adding a second `FAUX_BOLD_OFFSET`
/// pass when the text is meant to read bold but no real bold font was
/// installed. Every bold-weight paint in this module goes through here so
/// the degraded path can never be applied inconsistently.
fn paint_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    bold_weight: bool,
) -> egui::Rect {
    if bold_weight && !fonts::has_real_bold() {
        painter.text(
            pos + egui::vec2(FAUX_BOLD_OFFSET, 0.0),
            anchor,
            text,
            font.clone(),
            color,
        );
    }
    painter.text(pos, anchor, text, font, color)
}

/// `paint_text` shorthand for the common "bold at this scale size" case.
fn paint_bold_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    size: f32,
    color: egui::Color32,
) -> egui::Rect {
    paint_text(painter, pos, anchor, text, bold(size), color, true)
}

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
    /// The manual window move/resize gesture in flight, if any (issue #11);
    /// see `WindowGesture`. Lives on the app rather than in egui memory
    /// because it owns the reposition exemption guard, whose lifetime has
    /// to be explicit.
    window_gesture: WindowGesture,
    /// Collapse-to-header state (issue #54). Always starts expanded — it is
    /// deliberately not persisted to `Settings`; see the `CollapseState`
    /// section comment.
    collapse: CollapseState,
}

/// All icon textures the overlay paints, bundled so `OverlayApp` has exactly
/// one lazily-loaded field for them instead of one per icon set (issue #41).
struct Icons {
    classes: ClassIcons,
    toolbar: ToolbarIcons,
    glyphs: GlyphIcons,
}

impl Icons {
    fn load(ctx: &egui::Context) -> Self {
        Self {
            classes: ClassIcons::load(ctx),
            toolbar: ToolbarIcons::load(ctx),
            glyphs: GlyphIcons::load(ctx),
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
            window_gesture: WindowGesture::default(),
            collapse: CollapseState::default(),
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

        // Reconcile the collapse (issue #54) before anything is painted —
        // including the "somebody else resized us, expand" case that issue
        // #53's tray "Reset Window" reaches. The band height is the
        // collapsed window's whole inner height, and `draw_header` derives
        // the same number from the same call. The actual collapsed/expanded
        // read used to gate row-painting happens after `draw_header` below,
        // since that call can itself flip the state this frame.
        let band_height = header_band_height(
            encounter_subtitle(&self.snapshot.encounter).is_some(),
            ui.spacing().interact_size.y,
        );
        self.collapse.sync(&ctx, band_height);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(PANEL_FILL)
                    .stroke(egui::Stroke::new(PANEL_BORDER_WIDTH, PANEL_BORDER_COLOR))
                    .corner_radius(egui::CornerRadius::same(PANEL_CORNER_RADIUS)),
            )
            .show(ui, |ui| {
                // First, so the header buttons drawn afterwards stay on top of
                // the corner zones they overlap.
                draw_resize_handles(ui, &ctx, &mut self.window_gesture);
                draw_header(
                    ui,
                    &ctx,
                    &self.snapshot,
                    &self.tx_command,
                    SettingsHandle {
                        settings: &mut self.settings,
                        tx_settings: &self.tx_settings,
                    },
                    icons,
                    ChromeHandle {
                        gesture: &mut self.window_gesture,
                        collapse: &mut self.collapse,
                    },
                );
                // Read after `draw_header` returns, not before: the chevron
                // click it handles can flip the collapse state via
                // `chrome.collapse.toggle`, and gating row-painting below on
                // a value read before that call would still paint rows for
                // one extra frame after the click that just collapsed us.
                let collapsed = self.collapse.is_collapsed();
                // After both, so a gesture that started this frame is
                // already anchored — and, being outside them, it is the one
                // place a gesture can end no matter which zone began it.
                // The resize floor comes from the collapse state, not
                // `MIN_INNER_SIZE` directly (issue #54).
                drive_window_gesture(
                    &ctx,
                    &mut self.window_gesture,
                    self.collapse.min_inner_size(),
                );

                // Everything below the header band is skipped outright while
                // collapsed (issue #54) — not painted into a clipped rect —
                // so a collapsed overlay costs nothing per row, and the
                // window really is only as tall as the band it shows.
                if !collapsed {
                    if let StatusLine::Error(msg) = &self.status {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), msg.as_str());
                    }

                    ui.separator();
                    draw_rows(ui, &self.snapshot, &self.settings.ordered_columns(), icons);
                }
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

/// The persisted settings plus the channel that persists changes to disk,
/// bundled because every draw site that touches settings needs both —
/// mutating `settings` in place without also sending the update through
/// `tx_settings` would silently drop the change instead of writing it (see
/// `draw_header_menu`). Also what keeps `draw_header` under clippy's
/// too-many-arguments limit now that it takes a `WindowGesture` too.
struct SettingsHandle<'a> {
    settings: &'a mut Settings,
    tx_settings: &'a Sender<Settings>,
}

/// The two pieces of window-chrome state `draw_header`'s controls drive: the
/// in-flight manual move/resize gesture (issue #11, started by the title
/// bar's drag surface) and the collapse-to-header state (issue #54, toggled
/// by `draw_header_menu`'s Collapse/Expand item, not the chevron — the
/// chevron just opens the menu the item lives in, issue #71). Bundled for
/// exactly the reason `SettingsHandle` is —
/// `draw_header` is already at clippy's argument limit, and these two are
/// always needed together by the same function.
struct ChromeHandle<'a> {
    gesture: &'a mut WindowGesture,
    collapse: &'a mut CollapseState,
}

fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    chrome: ChromeHandle<'_>,
) {
    let title = encounter_title(&snapshot.encounter);
    let subtitle = encounter_subtitle(&snapshot.encounter);
    // Also the collapsed window's whole inner height (issue #54): collapsing
    // leaves exactly this band on screen, so the two are the same number by
    // construction rather than by a hardcoded constant that could drift.
    let band_height = header_band_height(subtitle.is_some(), ui.spacing().interact_size.y);

    // The panel's own full-width rect, captured before the band below
    // narrows it to the drag band's height — the background wash (issue #59,
    // #62) is sized off the panel, not the band, since it runs taller than
    // the band does on its own (`HEADER_WASH_HEIGHT` vs `band_height`).
    let panel = ui.available_rect_before_wrap();
    // The header band's full paint extent — `panel` truncated to
    // `band_height`, with no top adjustment. `RESIZE_EDGE` is a
    // hit-test-only concern (see `band` below), not a paint boundary; the
    // panel's own fill/border already cover that strip.
    let header_paint_clip =
        egui::Rect::from_min_size(panel.min, egui::vec2(panel.width(), band_height));
    // What the gutter emblem may paint into (issue #75): the header's *text*
    // rows only, not the whole band. Its 60pt box hangs well below a 20pt
    // title row, and the stat-pill row underneath has no gutter to hang into
    // — the timer half-pill is flush against the same left border — so ink
    // reaching that row lands straight on the pill, under a 6%-alpha fill
    // and behind a clock glyph tinted at a third alpha. Clipping to the
    // text band still keeps the intentional bleed off the left edge.
    let emblem_clip = egui::Rect::from_min_size(
        panel.min,
        egui::vec2(panel.width(), header_text_band_height(subtitle.is_some())),
    );
    // The whole header band is the drag surface — title line, the optional
    // subtitle line, and the timer/DPS/buttons row — registered *before* the
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
    // subtitle, stat row — sits on top of it.
    draw_header_wash(ui, panel, icons);

    let drag_surface = ui.interact(band, ui.id().with("title_bar"), egui::Sense::drag());
    if drag_surface.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::Grab);
    }
    // Once per gesture: this only captures the anchor the drag is measured
    // against. The actual per-frame repositioning is `drive_window_gesture`.
    if drag_surface.drag_started_by(egui::PointerButton::Primary) {
        begin_window_gesture(ctx, chrome.gesture, GestureKind::Move);
    }

    // Title is always rendered (even as the "No target" placeholder) so the
    // header's height never jitters between frames; the subtitle is omitted
    // entirely — not rendered blank — when the scene is unknown (issue #9
    // slice 2).
    let title_row = draw_title_line(ui, &title);
    // The gutter emblem (issue #59): bled off the left edge and centered on
    // the header's text rows, clipped to them (`emblem_clip`, issue #75) so
    // it stays clear of the stat-pill row below.
    if let Some(emblem) = icons.glyphs.get(GlyphIcon::Emblem) {
        ui.painter().with_clip_rect(emblem_clip).image(
            emblem.id(),
            header_emblem_rect(title_row, header_text_band_height(subtitle.is_some())),
            UV_FULL,
            HEADER_EMBLEM_COLOR,
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
    let chevron_response = menu_chevron(ui, chevron_rect(title_row));
    egui::Popup::menu(&chevron_response).show(|ui| {
        draw_header_menu(
            ui,
            ctx,
            tx_command,
            settings,
            icons,
            chrome.collapse,
            band_height,
        );
    });
    if let Some(subtitle) = &subtitle {
        draw_subtitle_line(ui, subtitle);
    }

    ui.horizontal(|ui| {
        // Three oval stat pills (issue #56), replacing the bare
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
        // Total damage for the fight (reference render's e.g. "30.1B"). The
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
        toggle_cluster(ui, icons);
    });
}

// -- status indicators (issue #62) ---------------------------------------
//
// The source's fourth stat-row cell: a click-through LED, a cloud-upload LED
// and a queue gauge, in a 22pt pill. We have none of those features, so
// these render **in their off state and are inert** — no click handling, no
// settings, no tooltip. Rendering them off is the honest reading: the
// source's `OffBrush` is `#1fff` for both LEDs, and an empty queue gauge is
// a bare ring with its check glyph, which is exactly the state a meter with
// no upload queue is in. Rendering them *on* would claim capabilities that
// do not exist. Issue #62 is explicit that no click-through or
// cloud-upload feature exists and a use for these slots will be decided
// later.

/// Tint every inert toggle glyph and the queue ring are painted with — the
/// source's `OffBrush="#1fff"`.
const TOGGLE_OFF_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x11);
const TOGGLE_MOUSE_SIDE: f32 = 12.0;
const TOGGLE_CLOUD_SIDE: f32 = 14.0;
const TOGGLE_QUEUE_SIDE: f32 = 14.0;
const TOGGLE_QUEUE_GLYPH_SIDE: f32 = 6.0;
const TOGGLE_GAP: f32 = 5.0;
const TOGGLE_PAD_X: f32 = 4.0;

/// Paints the three inert status toggles: a click-through LED (mouse-off), a
/// cloud-upload LED (cloud-off), and a queue gauge (an empty ring with a
/// check glyph — the source's backlog `Arc` overlay has nothing to draw with
/// no queue). All in one `PILL_FILL` oval, matching the DPS/damage pills'
/// chrome.
///
/// Allocated with `Sense::hover()` and its `Response` discarded — deliberately
/// no `on_hover_text`, no `widget_info`: a labelled inert control is worse
/// than an unlabelled decoration, since a screen reader announcing a
/// tooltip for a control that does nothing would be actively misleading.
fn toggle_cluster(ui: &mut egui::Ui, icons: &Icons) {
    let height = ui.spacing().interact_size.y;
    let width = 2.0 * TOGGLE_PAD_X
        + TOGGLE_MOUSE_SIDE
        + TOGGLE_GAP
        + TOGGLE_CLOUD_SIDE
        + TOGGLE_GAP
        + TOGGLE_QUEUE_SIDE;
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    // deliberately no tooltip / no widget_info — see the section comment
    // above.

    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    painter.rect_filled(rect, rect.height() / 2.0, PILL_FILL);

    let mut x = rect.left() + TOGGLE_PAD_X;
    let y = rect.center().y;

    if let Some(mouse_off) = icons.glyphs.get(GlyphIcon::MouseOff) {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(x + TOGGLE_MOUSE_SIDE / 2.0, y),
            egui::Vec2::splat(TOGGLE_MOUSE_SIDE),
        );
        painter.image(mouse_off.id(), icon_rect, UV_FULL, TOGGLE_OFF_COLOR);
    }
    x += TOGGLE_MOUSE_SIDE + TOGGLE_GAP;

    if let Some(cloud_off) = icons.glyphs.get(GlyphIcon::CloudOff) {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(x + TOGGLE_CLOUD_SIDE / 2.0, y),
            egui::Vec2::splat(TOGGLE_CLOUD_SIDE),
        );
        painter.image(cloud_off.id(), icon_rect, UV_FULL, TOGGLE_OFF_COLOR);
    }
    x += TOGGLE_CLOUD_SIDE + TOGGLE_GAP;

    // The queue gauge: an empty ring (the source's `Ellipse 14x14
    // Fill="#0aaa"` has alpha 0, so only the stroke is drawn) with the check
    // glyph centered in it. No backlog arc — with no queue there is nothing
    // for it to show.
    let queue_center = egui::pos2(x + TOGGLE_QUEUE_SIDE / 2.0, y);
    painter.circle_stroke(
        queue_center,
        TOGGLE_QUEUE_SIDE / 2.0,
        egui::Stroke::new(1.0, TOGGLE_OFF_COLOR),
    );
    if let Some(check) = icons.glyphs.get(GlyphIcon::Check) {
        let icon_rect =
            egui::Rect::from_center_size(queue_center, egui::Vec2::splat(TOGGLE_QUEUE_GLYPH_SIDE));
        painter.image(check.id(), icon_rect, UV_FULL, TOGGLE_OFF_COLOR);
    }
}

/// Fixed display size, in points, every toolbar icon (issue #41) is drawn
/// at — independent of the source PNGs' own resolution (48x48 in the
/// upstream ShinraMeter set), so a texture swap can never change a menu
/// item's or the chevron's footprint. Plus `apply_theme`'s
/// `button_padding.y` on both sides, this lands on
/// `egui::Style::default().spacing.interact_size.y` (18.0) — see this
/// module's own `toolbar_icon_button_height_matches_interact_size`.
const TOOLBAR_ICON_SIZE: f32 = 14.0;

/// Tint applied to every toolbar/stat icon — the source's footer buttons are
/// `Fill="White"` at content `Opacity=".5"`, i.e. white at half alpha, not a
/// slate-blue-gray recolor.
const TOOLBAR_ICON_TINT: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 128);

/// Builds an `egui::Image` for a loaded toolbar icon texture at the fixed
/// `TOOLBAR_ICON_SIZE`, overriding whatever size the source PNG itself
/// carries (`SizedTexture::from_handle` would use the PNG's native 48x48
/// instead), and multiplied by `TOOLBAR_ICON_TINT` so every icon reads at
/// the same half-white opacity regardless of its source color.
fn toolbar_icon_image(handle: &egui::TextureHandle) -> egui::Image<'static> {
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
fn menu_item_button<'a>(texture: Option<&egui::TextureHandle>, label: &'a str) -> egui::Button<'a> {
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
// affordance, not a collapse-state indicator — and the menu itself carries
// the Collapse/Expand action alongside the rest.

/// Side of the chevron's square hit/paint box, matched to `TOOLBAR_ICON_SIZE`
/// so it reads as one of the window controls rather than as decoration.
const CHEVRON_SIZE: f32 = TOOLBAR_ICON_SIZE;

/// Painted width of the V. The source's `ComboBoxToggleButton` chevron is a
/// `Path Width="10"`; the hit box stays `CHEVRON_SIZE` so the target is still
/// comfortable.
const CHEVRON_PAINT_WIDTH: f32 = 10.0;

/// Painted height of the V — a wide, shallow chevron, not an arrowhead.
const CHEVRON_PAINT_HEIGHT: f32 = 5.0;

/// The source's `Fill="#cfff"`.
const CHEVRON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0xCC);

/// Stroke width of the chevron. Thin, matching the reference's hairline
/// strokes, and a touch heavier than a hairline so it survives at 14pt.
const CHEVRON_STROKE: f32 = 1.5;

/// The chevron's square control box inside the title row's reserved
/// right-hand strip (`HEADER_RIGHT_CONTROL_WIDTH`, which `header_text_rect`
/// already keeps the title's own paint out of), centered in that strip both
/// ways.
///
/// Degrades rather than inverting at an absurdly narrow window, exactly like
/// `header_text_rect`: the strip is clamped against the row's left edge, and
/// the box is then clamped against the strip, so a hopeless width yields a
/// small-or-empty box inside the row instead of a backwards one.
fn chevron_rect(title_row: egui::Rect) -> egui::Rect {
    let left = (title_row.right() - HEADER_RIGHT_CONTROL_WIDTH).max(title_row.left());
    let strip = egui::Rect::from_min_max(
        egui::pos2(left, title_row.top()),
        egui::pos2(title_row.right(), title_row.bottom()),
    );
    let side = CHEVRON_SIZE.min(strip.width()).min(strip.height());
    egui::Rect::from_center_size(strip.center(), egui::Vec2::splat(side))
}

/// The three points of the chevron's polyline inside `rect`: a V opening
/// downward when `pointing_down`, and mirrored to open upward when not.
///
/// Down means "there is more below — click to fold it away" (the expanded
/// state, which is what the reference render shows); up means "click to
/// unfold" (collapsed). Pure, so the mirroring is unit-testable without a
/// painter — same reasoning as `pill_content_layout`.
fn chevron_points(rect: egui::Rect, pointing_down: bool) -> [egui::Pos2; 3] {
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
fn menu_chevron(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Response {
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

// -- stat pills (issue #56, #59, #62) ------------------------------------
//
// The reference render's header stats sit in fully-rounded oval containers:
// a barely-brighter translucent fill over the panel, no border stroke,
// generous horizontal padding, the value in bold white, and a small
// rasterized glyph. The same chrome is reused, at a smaller size, for issue
// #49's per-row death counter — which is why every knob below is a shared
// constant and the painter is one helper rather than three copies.
//
// Every glyph is now a real rasterized icon (`GlyphIcon`, `icons.rs`)
// blitted through `Painter::image`, not a hand-painted approximation —
// issue #59 vendors the source's actual stopwatch/speedometer/heart SVGs,
// so there is nothing left to paint procedurally.

/// Fill of a header/DPS/damage stat pill — the source's `#09ffffff`, white
/// at a very low alpha. Spelled premultiplied — white at alpha `a`
/// premultiplied is `(a, a, a, a)` — because `from_rgba_unmultiplied` is not
/// `const fn` in ecolor.
const PILL_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(9, 9, 9, 9);

/// Value text color inside a header/DPS/damage pill — the source's `#afff`:
/// white at ~2/3 alpha, the dimmer, partially transparent sibling of
/// `TITLE_TEXT_COLOR`'s opaque white. The two are deliberately not the same
/// value and must not be collapsed into one: the source keeps the encounter
/// title as the header's visually heaviest element and steps the stat values
/// down behind it, so painting the pills in full white would flatten that
/// hierarchy.
const PILL_VALUE_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0xAA);

/// Glyph tint inside a header/DPS/damage pill — the source's `#5bdf`, a
/// light steel blue distinct from `TOOLBAR_ICON_TINT`'s grayer slate: the
/// stat icons read as an accent, the window controls as chrome.
const PILL_ICON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xBB, 0xDD, 0xFF, 0x55);

/// Side of a header/DPS/damage pill's glyph box, in points — the source's
/// `GeneralStatPathStyle` `14x14`.
const PILL_GLYPH_SIDE: f32 = 14.0;

/// Timer half-pill fill — the source's `#1aaa`.
const TIMER_PILL_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xAA, 0xAA, 0xAA, 0x11);

/// Timer half-pill border — the source's `#2fff`.
const TIMER_PILL_BORDER: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x22);

/// Counter (death) pill fill — `MetricBorderStyle`'s `#1fff`.
const COUNTER_PILL_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x11);

/// Counter (death) pill glyph tint — `MetricPathStyle`'s `#5fff`. Dimmer,
/// via alpha rather than a darker gray, than the (now white)
/// `DEATH_COUNT_RGB` digits beside it.
const COUNTER_ICON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x55);

/// Side of a counter pill's glyph box, in points — the source's
/// `MetricPathStyle` `12x12`.
const COUNTER_GLYPH_SIDE: f32 = 12.0;

/// Horizontal padding inside a pill, on both ends. Generous on purpose —
/// it is most of what makes the oval read as a container rather than as a
/// highlight behind the text.
const PILL_PAD_X: f32 = 8.0;

/// Vertical padding above and below a pill's text. Small: the pill's height
/// is capped at the button row's height (`pill_size`) so the header band
/// budget (`header_band_height`) stays correct, and the text is what should
/// consume that budget.
const PILL_PAD_Y: f32 = 2.0;

/// Gap between a pill's value text and its icon.
const PILL_ICON_GAP: f32 = 5.0;

/// One stat pill's content. A struct rather than a long argument list
/// because issue #49's death counter and issue #59's timer half-pill need
/// the same chrome with different sizes, colors, glyph sides and corner
/// radii — a positional call would be unreadable at every call site.
struct StatPill<'a> {
    value: &'a str,
    /// The glyph texture, or `None` when its PNG failed to decode (never
    /// expected — the bytes are compile-time constants). `None` paints an
    /// empty icon box so the pill keeps the same width either way, exactly
    /// like `draw_row` reserves a class-icon slot for a class with no icon.
    icon: Option<egui::TextureId>,
    /// Side of the icon's square box, in points. Explicit rather than
    /// derived from the text's line height: the source fixes these per call
    /// site (`GeneralStatPathStyle` 14x14, `MetricPathStyle` 12x12).
    icon_side: f32,
    /// Point size of `value`.
    size: f32,
    value_color: egui::Color32,
    icon_color: egui::Color32,
    /// Icon before the value instead of after it. The header's DPS/damage
    /// pills read value-then-icon; the timer half-pill and issue #49's
    /// death counter read icon-then-value.
    icon_first: bool,
    /// Per-corner radius. The DPS/damage pills are full ovals; the timer
    /// half-pill is `CornerRadius="0 13 13 0"`, flush against the panel's
    /// left border.
    corner_radius: egui::CornerRadius,
    /// Fill behind the pill.
    fill: egui::Color32,
    /// Optional 1pt outline — only the timer half-pill has one (`#2fff`).
    stroke: Option<egui::Stroke>,
}

impl<'a> StatPill<'a> {
    /// A header stat pill (DPS or total damage): bold value in a light
    /// white, accent icon trailing it — the two ovals right of the timer in
    /// the reference's stat row.
    fn header(value: &'a str, icon: Option<egui::TextureId>) -> Self {
        Self {
            value,
            icon,
            icon_side: PILL_GLYPH_SIDE,
            size: FONT_SIZE_PILL_VALUE,
            value_color: PILL_VALUE_COLOR,
            icon_color: PILL_ICON_COLOR,
            icon_first: false,
            corner_radius: egui::CornerRadius::same((BUTTON_ROW_HEIGHT / 2.0) as u8),
            fill: PILL_FILL,
            stroke: None,
        }
    }

    /// The encounter-duration half-pill, flush against the panel's left
    /// border (`CornerRadius="0 13 13 0"`) — the source's largest stat text
    /// and its only stroked/bordered pill.
    fn timer(value: &'a str, icon: Option<egui::TextureId>) -> Self {
        Self {
            value,
            icon,
            icon_side: PILL_GLYPH_SIDE,
            size: FONT_SIZE_TIMER,
            value_color: PILL_VALUE_COLOR,
            icon_color: PILL_ICON_COLOR,
            icon_first: true,
            corner_radius: egui::CornerRadius {
                nw: 0,
                ne: 13,
                sw: 0,
                se: 13,
            },
            fill: TIMER_PILL_FILL,
            stroke: Some(egui::Stroke::new(1.0, TIMER_PILL_BORDER)),
        }
    }

    /// A per-row counter pill (issue #49's death column): the same oval
    /// chrome as `header`, at the counter size and led by its icon — the
    /// reference render reads skull-then-count, the opposite order to the
    /// header's value-then-icon.
    ///
    /// `value_color` comes from the column's own `StatColumn::color` rather
    /// than being fixed here, so the one place a column's color is declared
    /// stays `ColumnKind::spec` for the pill column too, exactly as it is
    /// for every text column.
    fn counter(value: &'a str, icon: Option<egui::TextureId>, value_color: egui::Color32) -> Self {
        Self {
            value,
            icon,
            icon_side: COUNTER_GLYPH_SIDE,
            size: FONT_SIZE_COUNTER,
            value_color,
            icon_color: COUNTER_ICON_COLOR,
            icon_first: true,
            corner_radius: egui::CornerRadius::same(12),
            fill: COUNTER_PILL_FILL,
            stroke: None,
        }
    }
}

/// RGB of the death count's digits (issue #49, issue #62) —
/// `ColumnKind::spec`'s color for `ColumnKind::Deaths`, declared here with
/// `CRIT_PCT_RGB`/`LUCKY_PCT_RGB` so every column color lives in the
/// painting module. The source's `DeathsDT` is plain `Foreground="White"` —
/// the pill's own `#1fff` background is what separates it, not a dimmer
/// digit color.
pub(crate) const DEATH_COUNT_RGB: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// Outer size of a pill holding text of `text_size` with a glyph box of
/// `icon_side`, capped at `max_height`.
///
/// The cap is load-bearing rather than cosmetic: the pills live in
/// `draw_header`'s button row, whose height `header_band_height` budgets as
/// `BUTTON_ROW_HEIGHT`. A pill taller than that would silently grow the
/// header band past the drag surface `draw_header` registered for it.
fn pill_size(text_size: egui::Vec2, icon_side: f32, max_height: f32) -> egui::Vec2 {
    let width = 2.0 * PILL_PAD_X + text_size.x + PILL_ICON_GAP + icon_side;
    let height = (text_size.y + 2.0 * PILL_PAD_Y).min(max_height);
    egui::vec2(width, height)
}

/// Where a pill's two pieces go inside its rect: the value text's
/// `Align2::LEFT_CENTER` anchor, and the icon's (square, vertically
/// centered) box. Pure geometry so both orderings are unit-testable without
/// a live `egui::Ui` — same reasoning as `icon_slot`.
fn pill_content_layout(
    rect: egui::Rect,
    text_size: egui::Vec2,
    icon_side: f32,
    icon_first: bool,
) -> (egui::Pos2, egui::Rect) {
    let left = rect.left() + PILL_PAD_X;
    let (text_x, icon_x) = if icon_first {
        (left + icon_side + PILL_ICON_GAP, left)
    } else {
        (left, left + text_size.x + PILL_ICON_GAP)
    };
    let y = rect.center().y;
    (
        egui::pos2(text_x, y),
        egui::Rect::from_center_size(
            egui::pos2(icon_x + icon_side / 2.0, y),
            egui::Vec2::splat(icon_side),
        ),
    )
}

/// Paints one oval stat pill and returns its `Response` (hover-only: none of
/// these are click targets — the reference's three *circular* buttons at the
/// right of the same row are inert status toggles for features this app
/// doesn't have; see `toggle_cluster`).
fn stat_pill(ui: &mut egui::Ui, pill: StatPill<'_>) -> egui::Response {
    let text_size = pill_text_size(ui.painter(), &pill);
    let size = pill_size(text_size, pill.icon_side, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());

    if ui.is_rect_visible(rect) {
        paint_stat_pill(ui.painter(), rect, text_size, &pill);
    }
    response
}

/// The laid-out size of a pill's value text. Split out of `stat_pill` so the
/// pill's *measurement* and its *painting* can be driven from a bare
/// `Painter` too — `draw_row`'s counter pill (issue #49) has no `Ui` of its
/// own to allocate from, it paints into a row rect that was already
/// allocated, at an x the column anchors dictate.
fn pill_text_size(painter: &egui::Painter, pill: &StatPill<'_>) -> egui::Vec2 {
    let mut size = painter
        .layout_no_wrap(pill.value.to_owned(), bold(pill.size), pill.value_color)
        .rect
        .size();
    // The value is painted through `paint_bold_text`, which on the faux-bold
    // path lays the same galley down twice, the second pass `FAUX_BOLD_OFFSET`
    // to the right — so what lands on screen is that much wider than what
    // epaint measured. Counting it here keeps the pill's width and its
    // icon's x (both derived from this) matching the ink instead of eating
    // the difference out of `PILL_ICON_GAP`.
    if !fonts::has_real_bold() {
        size.x += FAUX_BOLD_OFFSET;
    }
    size
}

/// Paints a pill's fill, optional stroke, value and icon into `rect`. The
/// layout half of `stat_pill`, with no `Ui` and therefore no allocation —
/// see `pill_text_size` for why the two are separate.
fn paint_stat_pill(
    painter: &egui::Painter,
    rect: egui::Rect,
    text_size: egui::Vec2,
    pill: &StatPill<'_>,
) {
    painter.rect_filled(rect, pill.corner_radius, pill.fill);
    if let Some(stroke) = pill.stroke {
        painter.rect_stroke(rect, pill.corner_radius, stroke, egui::StrokeKind::Inside);
    }
    let (text_pos, icon_rect) =
        pill_content_layout(rect, text_size, pill.icon_side, pill.icon_first);
    paint_bold_text(
        painter,
        text_pos,
        egui::Align2::LEFT_CENTER,
        pill.value,
        pill.size,
        pill.value_color,
    );
    if let Some(id) = pill.icon {
        painter.image(id, icon_rect, UV_FULL, pill.icon_color);
    }
}

/// The whole of a texture, in normalized texture coordinates — the `uv`
/// argument every full-texture `Painter::image` blit in this module passes.
const UV_FULL: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

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

/// White the title line is painted in — the source's inherited `White`
/// title foreground, deliberately not `ui.visuals().text_color()` (the
/// theme's default, dimmer body-text white) since the title needs to read
/// as the visually heaviest element in the header. `draw_title_line` is its
/// only user: the header stat pills, which once shared it, are painted a
/// step dimmer in `PILL_VALUE_COLOR` so the title still outweighs them.
const TITLE_TEXT_COLOR: egui::Color32 = egui::Color32::WHITE;

/// Height of the header's subtitle line. Not part of `default_inner_height`
/// — the subtitle is conditional and the default window assumes it is
/// absent (see `default_inner_height`'s doc).
///
/// `TITLE_LINE_HEIGHT (20) + ITEM_SPACING_Y (2) + SUBTITLE_LINE_HEIGHT (14)
/// == 36.0`, the source's `Height="36"` header grid, exactly.
const SUBTITLE_LINE_HEIGHT: f32 = 14.0;

/// Subtitle text color — the source's `#5fff`, white at ~1/3 alpha.
const SUBTITLE_TEXT_COLOR: egui::Color32 =
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
const HEADER_GUTTER_WIDTH: f32 = 34.0;
/// The source title/subtitle `Margin="2 … 0 0"` — a hair of air between the
/// gutter and the text.
const HEADER_TEXT_PAD_X: f32 = 2.0;

/// Width reserved at the *right* end of the title/subtitle rows — the
/// source's `ComboBoxToggleButton` chevron column, `Width="32"`.
///
/// Issue #54's collapse chevron is what occupies that strip — `chevron_rect`
/// centers its box in exactly this width, on the title row.
const HEADER_RIGHT_CONTROL_WIDTH: f32 = 32.0;

/// The sub-rect of a header row that title/subtitle text may actually paint
/// into: indented on the left by the fixed `HEADER_GUTTER_WIDTH` +
/// `HEADER_TEXT_PAD_X`, and stopping short of the right edge by
/// `HEADER_RIGHT_CONTROL_WIDTH`. Never inverted — at an absurdly narrow
/// width the right edge collapses onto the left one, giving an empty (not
/// negative) rect, which clips the text away entirely rather than painting
/// it backwards.
fn header_text_rect(row: egui::Rect) -> egui::Rect {
    let left = row.left() + HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;
    let right = (row.right() - HEADER_RIGHT_CONTROL_WIDTH).max(left);
    egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()))
}

/// Height of `draw_header`'s drag band: the title line, the optional
/// subtitle line, and the button row (`button_row_height`, egui's
/// `interact_size.y`), plus one `ITEM_SPACING_Y` gap for every adjacent pair
/// egui's vertical layout stacks them as — 1 gap (title -> button row) when
/// there's no subtitle, 2 (title -> subtitle -> button row) when there is.
/// Extracted from `draw_header` so the two cases are unit-testable without a
/// live `egui::Ui`.
fn header_band_height(has_subtitle: bool, button_row_height: f32) -> f32 {
    header_text_band_height(has_subtitle) + ITEM_SPACING_Y + button_row_height
}

/// Height of the header's *text* rows alone: the title line plus, when the
/// scene is known, the gap and the subtitle line under it — the source's
/// `Height="36"` header grid.
///
/// This, not the whole drag band, is the block the left gutter belongs to:
/// `header_text_rect` indents these rows by `HEADER_GUTTER_WIDTH` to leave
/// room for the emblem, while the stat-pill row below them has no gutter at
/// all — its timer half-pill is deliberately flush against the panel's left
/// border, in exactly the column the emblem occupies. So the emblem is both
/// centered in this height and clipped to it (issue #75); a clip to the full
/// band instead let its ink paint straight through the timer pill, whose
/// fill is 6% alpha and whose clock glyph is tinted at a third alpha.
fn header_text_band_height(has_subtitle: bool) -> f32 {
    if has_subtitle {
        TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT
    } else {
        TITLE_LINE_HEIGHT
    }
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
fn draw_title_line(ui: &mut egui::Ui, text: &str) -> egui::Rect {
    let desired_size = egui::vec2(ui.available_width(), TITLE_LINE_HEIGHT);
    let (row, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let rect = header_text_rect(row);
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
/// left edge by `Margin="-26 0 0 -8"` and clipped to the header band, so only
/// its right two-thirds are ever on screen.
const HEADER_EMBLEM_SIZE: f32 = 60.0;
/// The `Margin`'s left component: the emblem hangs 26pt off the left edge of
/// the header rows, so only its right `60 - 26 = 34`pt — one
/// `HEADER_GUTTER_WIDTH` — is ever on screen.
const HEADER_EMBLEM_LEFT_BLEED: f32 = -26.0;
/// The `Margin`'s bottom component (`-8`): a *negative* bottom margin, which
/// in WPF adds to the height the emblem is centered in rather than moving
/// it. With the source's 36pt header grid that gives `(36 + 8 - 60)/2 = -8`,
/// i.e. a top edge 8pt above the grid — but the grid is only 36pt tall when
/// the subtitle line is present, so `header_emblem_rect` recomputes the
/// centering from the text band it is actually given instead of baking that
/// one case in (issue #75).
const HEADER_EMBLEM_BOTTOM_BLEED: f32 = 8.0;
/// `Fill="SlateGray"`.
const HEADER_EMBLEM_COLOR: egui::Color32 = egui::Color32::from_rgb(0x70, 0x80, 0x90);

/// Where the header emblem's 60x60 box sits: bled off the left of the title
/// row `row`, and vertically centered on the header's text band
/// (`text_band_height`, plus the source's negative bottom margin) that
/// starts at that row's top. Pure geometry, so the negative-margin bleed is
/// unit-testable without a painter.
fn header_emblem_rect(row: egui::Rect, text_band_height: f32) -> egui::Rect {
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
const HEADER_WASH_HEIGHT: f32 = 98.0;
/// Inset from the panel's edges the wash is painted at, so its square
/// corners never poke past the panel's own `PANEL_CORNER_RADIUS`-rounded,
/// `PANEL_BORDER_WIDTH`-thick border.
const HEADER_WASH_INSET: f32 = 1.0;
/// Alpha at the wash gradient's brightest (top-left) stop — `Opacity=".5"`.
const HEADER_WASH_TOP_ALPHA: u8 = 0x50;
/// Side of the wash's oversized `Svg.HPBar` box, in points — the same emblem
/// the gutter draws at 60pt (`HEADER_EMBLEM_SIZE`), blown up as wallpaper.
const HEADER_WASH_EMBLEM_SIZE: f32 = 200.0;
/// How far the wash emblem's right edge overhangs the wash's own right edge,
/// in points: the source right-aligns the wash `Svg.HPBar` with a `-25` right
/// margin, so its last 25pt hang off the panel and the wash's clip rect cuts
/// them away — the mirror of the gutter emblem's `-26` left bleed
/// (`HEADER_EMBLEM_OFFSET`).
const HEADER_WASH_EMBLEM_BLEED: f32 = 25.0;
/// `Opacity=".05"` on a SlateGray fill.
const HEADER_WASH_EMBLEM_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x70, 0x80, 0x90, 13);

/// Where the wash panel sits for a central panel of `panel`: inset from the
/// panel's left, top and right edges by `HEADER_WASH_INSET`, and running down
/// to its own fixed `HEADER_WASH_HEIGHT` rather than to the panel's bottom.
/// Pure geometry, so the inset and the fixed height are unit-testable without
/// a painter — the same factoring as `header_emblem_rect`.
fn header_wash_rect(panel: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        panel.min + egui::Vec2::splat(HEADER_WASH_INSET),
        egui::vec2(panel.width() - 2.0 * HEADER_WASH_INSET, HEADER_WASH_HEIGHT),
    )
}

/// Where the wash's oversized emblem sits inside a wash of `wash`: vertically
/// centered on it and right-aligned so exactly `HEADER_WASH_EMBLEM_BLEED`
/// points overhang its right edge. Taller than the wash as well as wider, so
/// both the overhang and the top/bottom overflow rely on the caller's clip
/// rect.
fn header_wash_emblem_rect(wash: egui::Rect) -> egui::Rect {
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
/// (not the drag band): the wash runs to `HEADER_WASH_HEIGHT`, which is
/// taller than the band itself.
///
/// The source rounds the wash's top corners (`CornerRadius="7 7 0 0"`); egui
/// cannot clip to a rounded rect this cheaply, so the wash keeps square
/// corners — at alpha `0x50` under the panel's own 8pt-rounded, 1pt border
/// the difference is sub-pixel.
fn draw_header_wash(ui: &egui::Ui, panel: egui::Rect, icons: &Icons) {
    let wash_rect = header_wash_rect(panel);
    let painter = ui.painter().with_clip_rect(wash_rect);

    // Top-left brightest, fading to zero at the bottom-right — the source's
    // `LinearGradientBrush` with no explicit start/end points defaults to
    // that diagonal.
    let slate = |a: u8| egui::Color32::from_rgba_unmultiplied(0x70, 0x80, 0x90, a);
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
        painter.image(emblem.id(), emblem_rect, UV_FULL, HEADER_WASH_EMBLEM_COLOR);
    }
}

/// Color of the fading separator line painted under the header title
/// (`title_separator_segments`) — the source's `#708090`.
const TITLE_SEPARATOR_RGB: (u8, u8, u8) = (0x70, 0x80, 0x90);

/// Alpha the separator starts at, at its left (indented) end — the source's
/// left stop is a fully opaque `#708090`.
const TITLE_SEPARATOR_MAX_ALPHA: u8 = 255;

/// Thickness, in points, of the title separator line — `StrokeThickness="2"`.
const TITLE_SEPARATOR_THICKNESS: f32 = 2.0;

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
const TITLE_SEPARATOR_LEFT_BLEED: f32 = 5.0;

/// Number of thin strips `title_separator_segments` divides the fade into.
/// High enough to read as a smooth gradient, modest enough to stay cheap to
/// paint every frame.
const TITLE_SEPARATOR_SEGMENTS: usize = 24;

/// The rect the fading title separator is painted over, for a title row
/// `title_row`: it bleeds `TITLE_SEPARATOR_LEFT_BLEED` back into the gutter
/// from the title's own left edge and clears the chevron's reserved strip on
/// the right, sitting flush against the title row's bottom edge — the gap
/// between the title and subtitle rows in the reference render (see the
/// `TITLE_SEPARATOR_LEFT_BLEED` doc comment for why this isn't the source
/// margin's literal `7.5`).
fn title_separator_rect(title_row: egui::Rect) -> egui::Rect {
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
fn title_separator_segments(rect: egui::Rect) -> Vec<(egui::Rect, egui::Color32)> {
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

/// Paints the header's subtitle line (scene name/id), dimmed. Only called
/// when `encounter_subtitle` returned `Some` — the caller skips this
/// entirely, rather than calling it with empty text, so no space is
/// reserved when the scene is unknown.
fn draw_subtitle_line(ui: &mut egui::Ui, text: &str) {
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

/// The header dropdown (issue #54, #71): opened from `menu_chevron` via
/// `egui::Popup::menu`, replacing the old row of window-control icon
/// buttons. Built from plain `Ui` menu widgets rather than one bespoke
/// helper per item, since egui's own `menu_button`/`SubMenuButton` already
/// give every item free open/close state management.
///
/// Order matches the spec: Reset, a Columns submenu (issue #13's stat
/// column toggles, unchanged in behavior — just relocated), a separator,
/// Collapse/Expand (issue #54's collapse-to-header, still driven by
/// `CollapseState::toggle`), Minimize to tray, a separator, then Close.
///
/// The Columns submenu is given `PopupCloseBehavior::CloseOnClickOutside`
/// explicitly: the *root* popup's default `CloseOnClick` is fine for the
/// other items (a click on Reset/Collapse/Minimize/Close should dismiss the
/// whole menu), but the same default on the submenu would dismiss the
/// entire dropdown on every single checkbox toggle — verified against the
/// vendored egui source (`containers/menu.rs`'s `SubMenu::show`): the root
/// popup defers its own close decision to whichever submenu is open, so
/// only the submenu's own config matters for clicks landing on a checkbox.
fn draw_header_menu(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    collapse: &mut CollapseState,
    band_height: f32,
) {
    let SettingsHandle {
        settings,
        tx_settings,
    } = settings;
    if ui
        .add(menu_item_button(
            icons.toolbar.get(ToolbarIcon::Reset),
            "Reset",
        ))
        .clicked()
    {
        let _ = tx_command.try_send(UiCommand::Reset);
    }

    let columns_button = menu_item_button(icons.toolbar.get(ToolbarIcon::Settings), "Columns")
        .right_text(egui::containers::menu::SubMenuButton::RIGHT_ARROW);
    egui::containers::menu::SubMenuButton::from_button(columns_button)
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
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

    let collapse_label = if collapse.is_collapsed() {
        "Expand"
    } else {
        "Collapse"
    };
    if ui.button(collapse_label).clicked() {
        collapse.toggle(ctx, band_height);
    }

    // Issue #53: this minimize goes to the notification area, not the
    // taskbar. `platform::install_tray`'s subclass intercepts the
    // `WM_SIZE`/`SIZE_MINIMIZED` this command produces, adds a tray icon
    // and hides the window, so no call-site change is needed here — but the
    // tray icon is now the *only* way back, so don't route this through
    // anything that bypasses a real minimize.
    if ui.button("Minimize to tray").clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
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
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

/// Smallest inner size the overlay may be resized to, in points. Shared by
/// `viewport`'s `with_min_inner_size` — which is what stops winit/the OS
/// going smaller — and `resized_window_rect`'s clamp, so a manual resize
/// stops at exactly the same size the window would have been pinned to
/// anyway (a mismatch would leave the dragged edge visibly detached from
/// the pointer).
const MIN_INNER_SIZE: egui::Vec2 = egui::vec2(220.0, 90.0);

/// How far, in points, a gesture's target rect must differ from where the
/// window already is before a viewport command is worth sending. Purely to
/// keep a held-still drag from re-queueing identical commands every frame.
const GESTURE_EPSILON: f32 = 0.5;

// -- collapse to header (issue #54) ------------------------------------
//
// Collapsing hides the player rows (and the status banner) and shrinks the
// window down to the header band, so the overlay parks as a one-line strip.
// Expanding restores the height it had immediately before.
//
// Two things make this more than a boolean:
//
//  1. The collapsed height is **below `MIN_INNER_SIZE.y`** (the band is
//     40-58pt against a 90pt floor), and that floor is enforced in two
//     independent places — `viewport`'s `with_min_inner_size`, i.e. winit and
//     the OS, and `resized_window_rect`'s clamp on the manual drag-resize
//     (issue #11). Both are moved for the duration of the collapse rather
//     than lowered globally: a permanently lower floor would let a user drag
//     the *expanded* window down into an unusable sliver.
//  2. Something else can resize the window while it is collapsed — issue
//     #53's tray "Reset Window", which puts the overlay back at
//     `viewport`'s expanded default size, or a vertical drag-resize. The
//     collapsed state watches for a height it did not ask for and expands,
//     keeping the height whoever changed it chose. Without that, "Reset
//     Window" would produce a full-height window painting nothing but a
//     header.
//
// Deliberately **not persisted** to `Settings`: the issue doesn't ask for it,
// and a meter that reopens collapsed after a crash looks broken. Every launch
// starts expanded.

/// How far, in points, the window's real inner height may differ from the
/// collapsed height that was requested before it counts as somebody else's
/// change. Same job as `GESTURE_EPSILON`: absorb the sub-point wobble
/// fractional DPI scaling introduces between what is asked for and what
/// winit reports back.
const COLLAPSE_HEIGHT_EPSILON: f32 = 1.0;

/// The window's inner rect this frame, in points. The overlay is borderless
/// (`viewport`'s `with_decorations(false)`), so this is also its outer size —
/// and its origin is the window's own top-left, so only the size is
/// meaningful. Named because the collapse arithmetic asks for it three
/// times and `i.viewport_rect()` reads like screen geometry rather than
/// window size.
fn inner_rect(ctx: &egui::Context) -> egui::Rect {
    ctx.input(|i| i.viewport_rect())
}

/// Whether the overlay is collapsed to its header band, and everything needed
/// to put it back (issue #54). `None` is the expanded state.
#[derive(Debug, Default)]
pub struct CollapseState {
    collapsed: Option<Collapsed>,
}

/// The live collapse, while one is in effect.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Collapsed {
    /// The window's inner height, in points, at the moment it collapsed —
    /// restored verbatim when the chevron expands it again, so a
    /// collapse/expand round-trip is size-neutral even if the user had
    /// dragged the window to an unusual height first.
    ///
    /// Held here rather than read back off the live window on expand,
    /// because while collapsed the live window *is* the band height — there
    /// would be nothing left to read.
    restore_height: f32,
    /// The collapsed inner height last asked for via
    /// `ViewportCommand::InnerSize`. Not a constant: the header band grows
    /// and shrinks with the dungeon subtitle (`header_band_height`), which
    /// can appear or disappear mid-collapse when the encounter changes, so
    /// this is re-derived every frame and re-requested when it moves.
    requested_height: f32,
    /// Whether the window has actually reached `requested_height` yet.
    /// Viewport commands are queued and applied by winit later, so for the
    /// first frame or two after a collapse the window is still its old
    /// height — which must not be mistaken for somebody else resizing it.
    settled: bool,
}

/// What `CollapseState::sync` should do with the collapse this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CollapseSync {
    /// Nothing: the window is the height we asked for, or is still on its
    /// way there.
    Hold,
    /// The window has arrived at the requested height — mark it settled, so
    /// any *later* deviation is recognizable as somebody else's.
    Settle,
    /// The band's own height changed underneath the collapse (the dungeon
    /// subtitle appeared or disappeared); ask for the new one and wait for
    /// it to land.
    Request(f32),
    /// The window is a height nobody here asked for — issue #53's "Reset
    /// Window", or a vertical drag-resize — so honor it and expand.
    Expand,
}

/// Decides `CollapseSync` from the collapse's own bookkeeping, the header
/// band's current height, and the window's real inner height. Pure, so every
/// branch — including the ones that need a tray menu or a drag gesture to
/// reach in the running app — is unit-testable.
fn collapse_sync(collapsed: Collapsed, band_height: f32, actual_height: f32) -> CollapseSync {
    // Checked first: a band that changed size makes the old
    // `requested_height` stale, so the height comparison below would read
    // "somebody resized us" for what is really our own target moving.
    if (band_height - collapsed.requested_height).abs() > COLLAPSE_HEIGHT_EPSILON {
        return CollapseSync::Request(band_height);
    }
    let at_target = (actual_height - collapsed.requested_height).abs() <= COLLAPSE_HEIGHT_EPSILON;
    match (collapsed.settled, at_target) {
        (false, true) => CollapseSync::Settle,
        (true, false) => CollapseSync::Expand,
        _ => CollapseSync::Hold,
    }
}

impl CollapseState {
    /// Whether the player rows are currently hidden. `OverlayApp::ui` skips
    /// painting them (and the status banner, and the separator) entirely on
    /// this, rather than painting into a clipped-away rect.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed.is_some()
    }

    /// The minimum inner size in force right now: the normal floor while
    /// expanded, and one lowered to the collapsed height while collapsed.
    ///
    /// Handed to `resized_window_rect` so the manual drag-resize clamps at
    /// exactly the same floor the OS is being told about — otherwise a
    /// purely *horizontal* drag on a collapsed window would clamp its height
    /// back up to 90pt as a side effect and silently expand it.
    fn min_inner_size(&self) -> egui::Vec2 {
        match self.collapsed {
            Some(collapsed) => egui::vec2(MIN_INNER_SIZE.x, collapsed.requested_height),
            None => MIN_INNER_SIZE,
        }
    }

    /// Collapses or expands, moving the OS min-inner-size floor along with
    /// the window so winit doesn't refuse the sub-minimum collapsed height.
    ///
    /// The floor command is queued *ahead of* the size command on both
    /// paths — the lowered floor before the smaller size on collapse, the
    /// normal floor before the restored size on expand — so the window is
    /// never asked for a size the floor then in force would reject.
    fn toggle(&mut self, ctx: &egui::Context, band_height: f32) {
        let inner = inner_rect(ctx);
        match self.collapsed.take() {
            Some(collapsed) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(MIN_INNER_SIZE));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    inner.width(),
                    collapsed.restore_height,
                )));
                // Issue #74: an app-driven resize outside `disable_aero_snap`'s
                // one startup call never re-triggers DWM's frame recompute on
                // its own, leaving the header opaque gray. Fired once here —
                // `toggle` runs once per chevron click, never per frame — see
                // `platform::force_frame_recompute`'s doc comment.
                crate::platform::force_frame_recompute();
            }
            None => {
                self.collapsed = Some(Collapsed {
                    restore_height: inner.height(),
                    requested_height: band_height,
                    settled: false,
                });
                self.request_height(ctx, band_height);
            }
        }
    }

    /// Reconciles the collapse against the window once per frame. A no-op
    /// while expanded.
    fn sync(&mut self, ctx: &egui::Context, band_height: f32) {
        let Some(collapsed) = self.collapsed else {
            return;
        };
        match collapse_sync(collapsed, band_height, inner_rect(ctx).height()) {
            CollapseSync::Hold => {}
            CollapseSync::Settle => {
                self.collapsed = Some(Collapsed {
                    settled: true,
                    ..collapsed
                });
            }
            CollapseSync::Request(height) => {
                self.collapsed = Some(Collapsed {
                    requested_height: height,
                    settled: false,
                    ..collapsed
                });
                self.request_height(ctx, height);
            }
            CollapseSync::Expand => {
                // The window's *current* height is kept deliberately —
                // whoever set it (the tray's "Reset Window", or the user's
                // own drag) meant it, and overwriting it with
                // `restore_height` would undo their action. Only the chevron
                // restores the pre-collapse height.
                self.collapsed = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(MIN_INNER_SIZE));
            }
        }
    }

    /// Asks the window for a collapsed inner height of `height`, lowering the
    /// min-inner-size floor to match first. Width is left exactly as it is —
    /// collapsing is a vertical operation only.
    fn request_height(&self, ctx: &egui::Context, height: f32) {
        let width = inner_rect(ctx).width();
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
            MIN_INNER_SIZE.x,
            height,
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
        // Issue #74: see the identical call (and comment) in `toggle` above.
        // This fires once per call here too — `sync`'s `CollapseSync::Request`
        // branch that reaches this only runs when the band height has
        // actually changed, not every frame while collapsed.
        crate::platform::force_frame_recompute();
    }
}

/// What a manual window gesture is currently driving.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GestureKind {
    /// The header drag band: the whole window follows the pointer.
    Move,
    /// One of `resize_zones`' eight strips.
    Resize(egui::ResizeDirection),
}

/// A manual, app-driven window move or resize in progress (issue #11).
///
/// The OS-loop commands (`ViewportCommand::StartDrag` / `BeginResize`) are
/// deliberately *not* used: they hand the gesture to Windows' `SC_MOVE` /
/// `SC_SIZE` modal loops, which is the only place Aero Snap engages. So the
/// overlay tracks the pointer itself and repositions/resizes the window
/// frame by frame instead, and Snap never gets a loop to hook.
///
/// One instance is shared by the header and all eight resize zones, since
/// egui only ever drags one widget at a time — which also means there is
/// exactly one owner of the reposition exemption.
#[derive(Debug, Default)]
pub struct WindowGesture {
    active: Option<ActiveGesture>,
}

#[derive(Debug)]
struct ActiveGesture {
    kind: GestureKind,
    /// Pointer position in *screen* points when the gesture began, sourced
    /// from the OS cursor (`platform::cursor_position`) rather than
    /// reconstructed from the window's own rect — see `window_and_pointer`'s
    /// doc comment (issue #68) for why the latter is a feedback loop.
    start_pointer: egui::Pos2,
    /// The window's outer rect, in points, when the gesture began. Every
    /// frame's target is computed from this plus the total pointer delta,
    /// never from the previous frame's rect, so rounding in what winit
    /// actually applied can't accumulate over a long drag.
    start_rect: egui::Rect,
    /// Held for the whole gesture so the snap-blocking subclass in
    /// `platform` doesn't veto our own repositioning when it happens to
    /// land within a couple of pixels of a monitor half. A per-call
    /// closure would be useless here: the viewport commands below are
    /// queued, and winit turns them into `SetWindowPos` later in the
    /// frame. Dropped by `WindowGesture::end`.
    _exemption: crate::platform::RepositionGuard,
}

impl WindowGesture {
    fn kind(&self) -> Option<GestureKind> {
        self.active.as_ref().map(|active| active.kind)
    }

    fn begin(&mut self, kind: GestureKind, start_pointer: egui::Pos2, start_rect: egui::Rect) {
        // Release whatever was running first, so a gesture that somehow
        // started without the previous one ending can't stack exemptions.
        self.end();
        self.active = Some(ActiveGesture {
            kind,
            start_pointer,
            start_rect,
            _exemption: crate::platform::begin_app_driven_reposition(),
        });
    }

    /// Ends the gesture and drops its exemption guard. Idempotent, because
    /// it is called from every exit path (see `drive_window_gesture`).
    fn end(&mut self) {
        self.active = None;
    }
}

/// The window's outer rect and the pointer, both in screen points.
///
/// Screen space is the only frame of reference a manual gesture can use, but
/// getting there needs care: egui reports the pointer in window-local
/// coordinates, and as a gesture moves the window that local position can go
/// stale for a whole frame or more — Windows doesn't synthesize a mouse-move
/// message when a window moves under a stationary cursor. The old fix for
/// that, `window.min + local`, was itself the bug (issue #68): it re-derives
/// screen position from the very window the gesture is dragging, so on a
/// frame where the local pointer is stale but `outer_rect.min` has already
/// advanced, the reconstruction drifts by exactly that advance — and since
/// `drive_window_gesture`'s delta feeds off this every frame, that drift
/// compounds into runaway acceleration for any gesture that moves the
/// window's origin (`Move`, and the resize directions that drag a
/// corner/edge through it).
///
/// So the pointer is sourced from `platform::cursor_position` — the OS's own
/// cursor position via `GetCursorPos`, which the window being dragged can't
/// perturb — and the window-relative reconstruction is only a fallback for
/// when that's unavailable (non-Windows dev hosts, where this gesture is
/// cosmetic anyway). See `gesture_pointer` for the actual choice, kept pure
/// and separate so it's testable without a window.
fn window_and_pointer(ctx: &egui::Context) -> Option<(egui::Rect, egui::Pos2)> {
    // The closure below only *reads* out of `i` and returns — nothing in it
    // may call back into `ctx`. `ctx.pixels_per_point()` is itself
    // `ctx.input(|i| ...)`, and `platform::cursor_position` can `log::warn!`
    // on failure, taking the logger lock; either one running while this
    // closure still held the input lock would be a nested `ctx.input()` on
    // the same thread, which egui warns can deadlock against a queued
    // writer. So the three values this needs are copied out here (including
    // the scale factor, read as the `InputState` field `ctx.pixels_per_point`
    // would have gone back through `ctx.input` for), the lock is dropped, and
    // the OS cursor is resolved afterward.
    //
    // That scale is the *effective* one — `zoom_factor * native` — because
    // `outer_rect` is: egui-winit divides the window's physical rect by
    // exactly this value to produce it. Converting `GetCursorPos`'s physical
    // pixels with the bare native factor instead would put the OS cursor in a
    // different space than the rect and the local pointer it is measured
    // against the moment a zoom is ever applied.
    let (window, local, pixels_per_point) = ctx.input(|i| {
        (
            i.viewport().outer_rect,
            i.pointer.latest_pos(),
            i.pixels_per_point,
        )
    });
    let (window, local) = (window?, local?);
    let os_cursor = crate::platform::cursor_position(pixels_per_point);
    Some((window, gesture_pointer(os_cursor, window, local)))
}

/// The screen-space pointer position a gesture should measure against:
/// `os_cursor` when the OS supplied one, otherwise `window`'s origin plus
/// egui's window-local `local` position.
///
/// Pure and separate from `window_and_pointer` so the regression this closes
/// (issue #68 — see `window_and_pointer`'s doc comment) is testable without
/// an `egui::Context` or a real window.
fn gesture_pointer(
    os_cursor: Option<egui::Pos2>,
    window: egui::Rect,
    local: egui::Pos2,
) -> egui::Pos2 {
    os_cursor.unwrap_or_else(|| window.min + local.to_vec2())
}

/// Issue #74: whether a gesture of `kind` ending should force a DWM frame
/// recompute (`platform::force_frame_recompute`). Only resizes actually
/// change the window's size, and the opaque-gray symptom this works around
/// is a DWM composition artifact left behind by a resize — a pure `Move`
/// gesture ending has nothing to recompute a frame for.
///
/// Pulled out of `drive_window_gesture` as a pure function, the same way
/// `row_bar_frac`/`share_bar_paints`/`column_anchors` extract pure decisions
/// out of `Ui`-dependent code elsewhere in this file, so this call-site
/// choice is unit-testable without a window.
fn gesture_end_needs_frame_recompute(kind: GestureKind) -> bool {
    matches!(kind, GestureKind::Resize(_))
}

/// Starts `kind` from wherever the pointer and window are right now.
fn begin_window_gesture(ctx: &egui::Context, gesture: &mut WindowGesture, kind: GestureKind) {
    // No position reported yet (a frame before winit has placed the
    // window) means there's no anchor to measure against; skipping just
    // costs the user one re-grab.
    if let Some((window, pointer)) = window_and_pointer(ctx) {
        log::debug!(
            "begin_window_gesture: {kind:?} start_rect={window:?} start_pointer={pointer:?}"
        );
        gesture.begin(kind, pointer, window);
    }
}

/// Advances the in-flight gesture by one frame, or ends it. Called once per
/// frame after the header and resize zones have had their chance to start
/// one.
///
/// `min_size` is the floor a resize clamps at. It is a parameter rather than
/// `MIN_INNER_SIZE` directly because issue #54's collapsed state lowers the
/// floor for as long as it lasts (`CollapseState::min_inner_size`) — a drag
/// clamping at the expanded 90pt minimum while the window is a 40pt strip
/// would fight the collapse on the very first frame of any drag, including a
/// purely horizontal one.
fn drive_window_gesture(ctx: &egui::Context, gesture: &mut WindowGesture, min_size: egui::Vec2) {
    let Some(kind) = gesture.kind() else {
        return;
    };
    let Some((start_pointer, start_rect)) = gesture
        .active
        .as_ref()
        .map(|active| (active.start_pointer, active.start_rect))
    else {
        return;
    };

    // The single release point for every way a gesture can end — pointer
    // released, drag cancelled, or the window losing focus mid-drag (an
    // alt-tab or the game grabbing focus back), which stops delivering
    // pointer state and would otherwise strand the exemption guard for the
    // rest of the session.
    let holding = ctx.input(|i| i.pointer.primary_down() && i.viewport().focused.unwrap_or(true));
    if !holding {
        log::debug!("drive_window_gesture: ending {kind:?}");
        gesture.end();
        // Issue #74: fired exactly once here, on the single frame a gesture
        // transitions from active to ended — the next frame `gesture.kind()`
        // is `None` and this whole function returns before reaching this
        // point again, so this can never fire on every frame of an
        // in-progress drag (which would risk reproducing issue #68's resize
        // runaway; see `platform::force_frame_recompute`'s doc comment).
        if gesture_end_needs_frame_recompute(kind) {
            crate::platform::force_frame_recompute();
        }
        return;
    }

    let Some((window, pointer)) = window_and_pointer(ctx) else {
        return;
    };
    let delta = pointer - start_pointer;
    let target = match kind {
        GestureKind::Move => {
            egui::Rect::from_min_size(moved_window_origin(start_rect, delta), start_rect.size())
        }
        GestureKind::Resize(direction) => {
            resized_window_rect(start_rect, direction, delta, min_size)
        }
    };

    if target.min.distance(window.min) > GESTURE_EPSILON {
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target.min));
    }
    // The overlay is borderless, so its outer and inner rects are the same
    // size; `InnerSize` is just the command egui exposes for setting it.
    if (target.size() - window.size()).length() > GESTURE_EPSILON {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(target.size()));
    }
}

/// Where the window's top-left corner goes for a move gesture: straight
/// offset by the total pointer delta since the drag began.
fn moved_window_origin(start: egui::Rect, delta: egui::Vec2) -> egui::Pos2 {
    start.min + delta
}

/// The window rect a resize gesture is asking for: `start` with the edges
/// `direction` names moved by `delta`, clamped so neither axis goes below
/// `min_size`.
///
/// Dragging a west/north edge changes the origin as well as the size, and
/// the clamp deliberately pushes back the edge the user is *holding* rather
/// than the one they aren't — otherwise a drag past the minimum would keep
/// shoving the anchored edge across the screen.
fn resized_window_rect(
    start: egui::Rect,
    direction: egui::ResizeDirection,
    delta: egui::Vec2,
    min_size: egui::Vec2,
) -> egui::Rect {
    use egui::ResizeDirection as Dir;

    let west = matches!(direction, Dir::West | Dir::NorthWest | Dir::SouthWest);
    let east = matches!(direction, Dir::East | Dir::NorthEast | Dir::SouthEast);
    let north = matches!(direction, Dir::North | Dir::NorthEast | Dir::NorthWest);
    let south = matches!(direction, Dir::South | Dir::SouthEast | Dir::SouthWest);

    let mut rect = start;
    if west {
        rect.min.x += delta.x;
    }
    if east {
        rect.max.x += delta.x;
    }
    if north {
        rect.min.y += delta.y;
    }
    if south {
        rect.max.y += delta.y;
    }

    if rect.width() < min_size.x {
        if west {
            rect.min.x = rect.max.x - min_size.x;
        } else {
            rect.max.x = rect.min.x + min_size.x;
        }
    }
    if rect.height() < min_size.y {
        if north {
            rect.min.y = rect.max.y - min_size.y;
        } else {
            rect.max.y = rect.min.y + min_size.y;
        }
    }
    rect
}

/// Width of the invisible edge strips that start a resize, in points.
///
/// Unaffected by the rounded `PANEL_CORNER_RADIUS` chrome: the rounding is
/// *painted* only — no OS window region is set, so the window is still a
/// rectangle to winit and to this hit test. The corner squares below still
/// cover a live grab area, so a user aiming just outside the visible arc
/// still resizes, and the 1pt border sits inside this 6pt north strip, so
/// dragging the visible border edge resizes, which is what a user expects.
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
/// own: invisible strips along the edges that start a manual resize gesture
/// (`WindowGesture`) rather than handing the window manager a native resize
/// loop, which on Windows is where Snap would engage.
fn draw_resize_handles(ui: &mut egui::Ui, ctx: &egui::Context, gesture: &mut WindowGesture) {
    let window = ctx.input(|i| i.viewport_rect());
    // `ResizeDirection` is not `Hash`, so the zone's position in the array is
    // what keeps the eight ids distinct.
    for (index, (zone, direction, cursor)) in resize_zones(window).into_iter().enumerate() {
        let handle = ui.interact(zone, ui.id().with(("resize", index)), egui::Sense::drag());
        if handle.hovered() {
            ctx.set_cursor_icon(cursor);
        }
        // Same as the title-bar drag: the anchor is captured once, then
        // `drive_window_gesture` does the per-frame work.
        if handle.drag_started_by(egui::PointerButton::Primary) {
            begin_window_gesture(ctx, gesture, GestureKind::Resize(direction));
        }
    }
}

/// Takes the whole `Icons` bundle rather than just `ClassIcons`: since issue
/// #49 a row paints a toolbar-set texture too (the death counter's skull),
/// and handing both sets down as one argument keeps `draw_row` from growing
/// a second icon parameter.
fn draw_rows(ui: &mut egui::Ui, snapshot: &Snapshot, columns: &[ColumnKind], icons: &Icons) {
    // True contiguous 30pt rows (decision 3): scoped to this function only,
    // so the header and menus keep `apply_theme`'s `item_spacing` — rows'
    // hover bands and accent lines must sit flush against their neighbors
    // with no gap, which a nonzero `item_spacing.y` would reintroduce.
    ui.spacing_mut().item_spacing.y = 0.0;

    // The enabled-column set (and therefore the column widths and their
    // anchors) is identical for every row in a frame, so both are computed
    // once here rather than once per row inside `draw_row`.
    let stat_columns = stat_columns_for(columns);
    let avail = ui.available_rect_before_wrap();
    let anchors = column_anchors(avail.left(), avail.right(), &stat_columns, 4.0);

    // Issue #73: each row's damage-share bar is now scaled to the *top
    // row's* damage rather than to its share of total encounter damage, so
    // the highest-damage row always fills the full bar width. `snapshot.rows`
    // is already sorted descending by damage (`Encounter::snapshot`), so the
    // top row's damage is just the first row's, computed once here rather
    // than once per row inside `draw_row`.
    let top_damage = snapshot.rows.first().map(|r| r.damage).unwrap_or(0);

    for row in &snapshot.rows {
        draw_row(ui, row, columns, &stat_columns, &anchors, icons, top_damage);
    }
}

/// How prominently one stat column's text is painted (issue #56). The
/// reference render is not flat: a row's DPS value is its headline number,
/// the percentages beside it are supporting color, and everything else sits
/// between the two. Derived from the `ColumnKind` rather than carried on
/// `StatColumn`, because `StatColumn` is built in `settings.rs`
/// (`ColumnKind::spec`) and typography belongs with the painting, not with
/// the column's width and formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnEmphasis {
    /// The headline number, e.g. DPS.
    Value,
    /// A plain stat, e.g. damage.
    Stat,
    /// A percentage, already colored by `StatColumn::color`.
    Percent,
    /// A counter (issue #49's death count): the one level that is **not
    /// painted as bare text** — `draw_row` wraps it in the same oval
    /// `stat_pill` chrome the header stats use, icon first. This is the
    /// dispatch point for that: `StatColumn` can only describe a string (a
    /// width, a formatter and a color), so the "paint this one differently"
    /// decision belongs with the typography, next to the sizes it shares,
    /// rather than as a bare `if kind == ColumnKind::Deaths` in the paint
    /// loop.
    Counter,
}

impl ColumnEmphasis {
    /// The font this emphasis level paints in. Every level is `FONT_SIZE_ROW`
    /// (the source's flat `MetricTextBlockStyle`) — row hierarchy is carried
    /// by color, not by size or weight, so this exists to keep the "which
    /// size" question centralized rather than to actually vary it.
    ///
    /// `Counter` reports the font its *pill* lays the value out in
    /// (`stat_pill` -> `pill_text_size` -> `bold(pill.size)`), not a font
    /// `draw_row` ever passes to `paint_text` — that is what lets the column
    /// width-budget tests measure the pill column the same way they measure
    /// every other one.
    fn font(self) -> egui::FontId {
        match self {
            Self::Value | Self::Stat | Self::Percent => regular(FONT_SIZE_ROW),
            Self::Counter => bold(FONT_SIZE_COUNTER),
        }
    }

    /// Whether `draw_row` paints this column as a pill rather than as text.
    fn is_pill(self) -> bool {
        matches!(self, Self::Counter)
    }
}

/// Maps a column to its emphasis level (issue #56).
fn column_emphasis(kind: ColumnKind) -> ColumnEmphasis {
    match kind {
        ColumnKind::Dps => ColumnEmphasis::Value,
        ColumnKind::SharePct | ColumnKind::CritPct | ColumnKind::LuckyPct => {
            ColumnEmphasis::Percent
        }
        // Issue #49. The only pill-painted column; see `ColumnEmphasis::
        // Counter`.
        ColumnKind::Deaths => ColumnEmphasis::Counter,
        ColumnKind::Damage
        | ColumnKind::Hits
        | ColumnKind::AbilityScore
        | ColumnKind::SeasonStrength => ColumnEmphasis::Stat,
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
    /// Text color this column is painted with. Only the DPS column (and
    /// the two unbudgeted stats with no source counterpart) stay pure
    /// white; plain stats use `STAT_TEXT_RGB` and `CritPct`/`LuckyPct` use
    /// `CRIT_PCT_RGB`/`LUCKY_PCT_RGB` to stand out the way the reference
    /// meter colors them.
    pub color: egui::Color32,
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

/// The horizontal clip rect for one stat column's painted text: bounded on
/// the right by that column's anchor (where its right-aligned text ends)
/// and on the left by exactly one nominal `width` — the budget in-range
/// text for the column is designed, and tested
/// (`widest_formatted_text_fits_its_column_width_budget`), to fit.
///
/// This is what keeps an out-of-range value (e.g. a packet-decoded
/// `ability_score`/`season_strength` past the in-game ceiling `StatColumn`'s
/// `width` budget assumes — see `ColumnKind::spec`) from painting
/// arbitrarily far left across the row: `draw_row` clips every column's text
/// draw to this rect rather than trusting the formatted string to fit
/// `width`, so an overlong string loses its leading glyphs after one
/// column's worth instead of running over its neighbors.
///
/// The slot is the *nominal* `width`, deliberately not the gap between this
/// column's anchor and the previous one. The two are identical whenever the
/// row is wide enough to hold every column at full width; in a narrower row
/// `column_anchors` scales those gaps down (see there), and clipping to a
/// scaled gap would cut ordinary in-range values short — right-aligned text
/// is clipped from the *left*, so a clipped `1000.0K` reads as a smaller
/// number rather than as damage. In that compressed case the slots overlap
/// and an overflowing value can bleed into its neighbor, which is exactly
/// what a too-narrow window did before any clipping existed; visible
/// overlap beats silently hiding digits. So the clip is a no-op only for
/// values that fit their budget in a row wide enough not to be compressed.
///
/// `Painter::with_clip_rect` intersects with the parent's clip rect, so a
/// slot reaching past the row's own left edge is still bounded by the ui.
fn column_clip_rect(rect: egui::Rect, anchor: f32, width: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(anchor - width, rect.top()),
        egui::pos2(anchor, rect.bottom()),
    )
}

fn draw_row(
    ui: &mut egui::Ui,
    row: &PlayerRow,
    kinds: &[ColumnKind],
    columns: &[StatColumn],
    anchors: &[f32],
    icons: &Icons,
    top_damage: i64,
) {
    let desired_size = egui::vec2(ui.available_width(), ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    // Background bar scaled by this row's damage relative to the *top row's*
    // damage, not this row's share of total encounter damage (issue #73) —
    // the highest-damage row always fills the full bar width. Painted before
    // (i.e. under) the icon and name; the icon slot is reserved on top of
    // it, not cut out of it. A vertically graded fill plus a horizontally
    // graded accent line along the bottom edge — the accent now matches the
    // fill's width rather than always spanning the full row — matching the
    // reference meter's gradients exactly (square corners — no rounding).
    let bar_frac = row_bar_frac(row.damage, top_damage);
    let paints = share_bar_paints(rect, bar_frac, row.class);
    ui.painter().add(egui::Shape::mesh(vertical_gradient_mesh(
        paints.fill_rect,
        egui::Color32::TRANSPARENT,
        paints.fill_bottom,
    )));
    ui.painter().add(egui::Shape::mesh(horizontal_gradient_mesh(
        paints.accent_rect,
        paints.accent_left,
        paints.accent_right,
    )));

    // Per-row hover highlight (decision 7): a horizontal gradient peaking
    // near the row's left edge, painted over the share bar and under the
    // icon/name/columns.
    if response.hovered() {
        for (quad, left, right) in row_hover_quads(rect) {
            ui.painter().add(egui::Shape::mesh(horizontal_gradient_mesh(
                quad, left, right,
            )));
        }
    }

    // The icon slot (issue #9) is reserved at a fixed offset regardless of
    // whether this row's class has an icon, so names stay left-aligned in a
    // column across rows either way — only the painting below is
    // conditional.
    let (icon_rect, name_offset) = icon_slot(rect);
    if let Some(texture) = row.class.and_then(|class| icons.classes.get(class)) {
        ui.painter()
            .image(texture.id(), icon_rect, UV_FULL, CLASS_ICON_TINT);
    }

    // Regular weight, at the row's flat metric size (issue #62): the
    // source's row text carries no `FontWeight`, name included — hierarchy
    // comes from color, and the name is already the brightest thing left of
    // the stat columns at plain white.
    let name = row_name(row);
    paint_text(
        ui.painter(),
        rect.left_center() + egui::vec2(name_offset, 0.0),
        egui::Align2::LEFT_CENTER,
        &name,
        regular(FONT_SIZE_ROW),
        egui::Color32::WHITE,
        false,
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
    // The death counter's skull (issue #49, issue #59), resolved once per
    // row rather than once per column: `GlyphIcons::get` is a linear scan,
    // and the texture is the same for every row in every frame. `None` (the
    // PNG failed to decode — never expected, the bytes are compile-time
    // constants) degrades to an empty icon box, see `StatPill::icon`.
    let skull = icons.glyphs.get(GlyphIcon::Skull).map(|t| t.id());

    for ((anchor_x, column), kind) in anchors.iter().zip(columns).zip(kinds) {
        let text = (column.text)(row);
        // Each column's own weight/size (issue #56, `column_emphasis`), not
        // one flat font for the whole row: the DPS value is the headline
        // number and the percentages are supporting detail. Numerals are
        // proportional now rather than monospace — the digits in both
        // egui's bundled font and the system faces `fonts.rs` installs are
        // tabular (equal advance), so right-aligned columns stay just as
        // steady as they did in monospace, and every column's text got
        // *narrower*, never wider, than the `StatColumn::width` budget it
        // was measured against.
        let emphasis = column_emphasis(*kind);
        // Clipped to this column's own slot (`column_clip_rect`) so a value
        // wider than the column's width budget (e.g. an out-of-range
        // `ability_score`/`season_strength` straight off the packet, with
        // no clamp anywhere upstream) is cut off after one column's worth
        // rather than painted across the columns to its left. The pill
        // column below is clipped by exactly the same rect — its chrome is
        // no more entitled to spill into its neighbor than text is.
        let painter = ui
            .painter()
            .with_clip_rect(column_clip_rect(rect, *anchor_x, column.width));
        if emphasis.is_pill() {
            paint_counter_pill(
                &painter,
                rect,
                *anchor_x,
                StatPill::counter(&text, skull, column.color),
            );
        } else {
            paint_text(
                &painter,
                egui::pos2(*anchor_x, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                &text,
                emphasis.font(),
                column.color,
                false,
            );
        }
    }
}

/// Paints one counter pill (issue #49) so that its **right edge lands on
/// `anchor`** — the same x a text column's `Align2::RIGHT_CENTER` paint would
/// end at — and its box is vertically centered in the row.
///
/// Anchoring the pill's edge where the text's edge would go is what makes the
/// pill column obey `column_anchors` exactly like every other column: the
/// anchor is a pure function of the row rect and the column widths, so the
/// pill neither shifts when the count gains a digit nor needs a width its
/// `StatColumn::width` budget doesn't already cover.
fn paint_counter_pill(painter: &egui::Painter, row: egui::Rect, anchor: f32, pill: StatPill<'_>) {
    let text_size = pill_text_size(painter, &pill);
    // Capped at the row's own height for the same reason the header's pills
    // are capped at the button row's (`pill_size`): a pill taller than its
    // container would overlap the rows above and below it.
    let size = pill_size(text_size, pill.icon_side, row.height());
    paint_stat_pill(
        painter,
        counter_pill_rect(row, anchor, size),
        text_size,
        &pill,
    );
}

/// Where a counter pill of `size` sits in `row` for a column anchored at
/// `anchor`. Pure geometry, so the right-alignment and centering are
/// unit-testable without a live painter — same reasoning as
/// `pill_content_layout`.
fn counter_pill_rect(row: egui::Rect, anchor: f32, size: egui::Vec2) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(anchor - size.x, row.center().y - size.y / 2.0),
        size,
    )
}

/// Per-role base RGB of the damage-share bar (issue #44: healer -> green,
/// tank -> blue, damage -> red). Split out from the alpha constants below
/// (issue #43) so the fill/accent alpha split and accent thickness stay
/// fixed regardless of which color a role uses — only the hue varies.
///
/// Sampled directly from the game's own class-icon tints
/// (`docs/reference/role-colors.webp`, issue #77) rather than hand-picked
/// for legibility — these are the exact hues the game itself uses for each
/// role's icon.
const SHARE_BAR_RGB_HEALER: (u8, u8, u8) = (131, 196, 154);
const SHARE_BAR_RGB_TANK: (u8, u8, u8) = (104, 166, 205);
const SHARE_BAR_RGB_DAMAGE: (u8, u8, u8) = (219, 135, 135);
/// Fallback for `Class::Unknown` (or a row with no `Class` at all). A
/// desaturated grey rather than any role's hue: reusing a role color here
/// (as this once did with `SHARE_BAR_RGB_TANK`'s blue) would make an
/// unclassified row indistinguishable from a confirmed row of that role, so
/// this must stay visually distinct from all three colors above (issue #44's
/// second open question).
const SHARE_BAR_RGB_UNKNOWN: (u8, u8, u8) = (140, 140, 140);

/// RGB for the `CritPct` stat column's text. Sampled directly from the
/// reference meter screenshots — `docs/reference/new-shinra-ex.webp` and
/// `docs/reference/tera_shinrameter_ex.png` both render their crit-%
/// column in this exact hex (`#F08080`, CSS "lightcoral"), so this is not
/// a guess.
pub(crate) const CRIT_PCT_RGB: (u8, u8, u8) = (240, 128, 128);
/// RGB for the `LuckyPct` stat column's text. Neither reference screenshot
/// has a visibly colored lucky-% column to sample, so this is *not*
/// sampled — it reuses `SHARE_BAR_RGB_HEALER`'s green as the nearest
/// existing convention for "this stat is good, color it green" rather than
/// inventing a new hue.
pub(crate) const LUCKY_PCT_RGB: (u8, u8, u8) = (70, 200, 120);
/// RGB of the plain (non-headline, non-percentage) stat columns — the
/// source's `DamagePercDT`/`DamageDT` `Foreground="#aaa"`. Only the DPS
/// column stays pure white; everything unremarkable is stepped down a
/// notch.
pub(crate) const STAT_TEXT_RGB: (u8, u8, u8) = (0xAA, 0xAA, 0xAA);

/// Alpha at the *bottom* of the bar fill's vertical gradient — the source's
/// `Opacity=".18"` bottom stop. The top stop is 0 (fully transparent).
const SHARE_BAR_FILL_BOTTOM_ALPHA: u8 = 46;

/// Alpha at the *left* end of the accent line's horizontal gradient — the
/// source's `Opacity=".1"` left stop. The right stop is fully opaque (255).
const SHARE_BAR_ACCENT_LEFT_ALPHA: u8 = 26;

/// Thickness of the accent line along the row's bottom edge (issue #43;
/// source `Height="2"`). `share_bar_paints` clamps this against the row
/// height so it stays sane — never taller than the row itself — at small
/// row heights.
const SHARE_BAR_ACCENT_THICKNESS: f32 = 2.0;

/// A two-triangle gradient quad. egui has no gradient brush, so the
/// source's `LinearGradientBrush`es are reproduced as meshes with
/// per-vertex colors — exact, one draw call, and cheaper than the strip-
/// stacking `title_separator_segments` uses.
fn gradient_mesh(
    rect: egui::Rect,
    tl: egui::Color32,
    tr: egui::Color32,
    bl: egui::Color32,
    br: egui::Color32,
) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), tl);
    mesh.colored_vertex(rect.right_top(), tr);
    mesh.colored_vertex(rect.left_bottom(), bl);
    mesh.colored_vertex(rect.right_bottom(), br);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    mesh
}

fn vertical_gradient_mesh(
    rect: egui::Rect,
    top: egui::Color32,
    bottom: egui::Color32,
) -> egui::Mesh {
    gradient_mesh(rect, top, top, bottom, bottom)
}

fn horizontal_gradient_mesh(
    rect: egui::Rect,
    left: egui::Color32,
    right: egui::Color32,
) -> egui::Mesh {
    gradient_mesh(rect, left, right, left, right)
}

/// The source's per-row hover band: a horizontal gradient from transparent,
/// up to `#1fff` at 15% across, and back to transparent at the right edge —
/// a highlight that peaks near the row's left edge rather than a flat fill.
const ROW_HOVER_PEAK_ALPHA: u8 = 17;
const ROW_HOVER_PEAK_OFFSET: f32 = 0.15;

/// The two gradient quads a hovered row's highlight is made of: transparent
/// -> peak over the first `ROW_HOVER_PEAK_OFFSET` of the width, then peak ->
/// transparent over the rest. Pure, so the split point is unit-testable
/// without a live `Ui` — same reasoning as `share_bar_paints`.
fn row_hover_quads(rect: egui::Rect) -> [(egui::Rect, egui::Color32, egui::Color32); 2] {
    let peak = egui::Color32::from_rgba_unmultiplied(255, 255, 255, ROW_HOVER_PEAK_ALPHA);
    let split_x = rect.left() + rect.width() * ROW_HOVER_PEAK_OFFSET;
    let left_quad = egui::Rect::from_min_max(rect.left_top(), egui::pos2(split_x, rect.bottom()));
    let right_quad = egui::Rect::from_min_max(egui::pos2(split_x, rect.top()), rect.right_bottom());
    [
        (left_quad, egui::Color32::TRANSPARENT, peak),
        (right_quad, peak, egui::Color32::TRANSPARENT),
    ]
}

/// The two paints that make up a row's damage-share bar: a share-scaled
/// fill, vertically graded transparent -> `fill_bottom`, and an accent line
/// of the same width, horizontally graded `accent_left` -> `accent_right`.
/// Named fields rather than a positional tuple so a fill/accent (or
/// rect/color) mix-up at the `draw_row` call site fails to compile instead
/// of silently swapping which paint lands where.
struct ShareBarPaints {
    /// The share-scaled fill, vertically graded transparent -> `fill_bottom`.
    fill_rect: egui::Rect,
    fill_bottom: egui::Color32,
    /// The accent line. Issue #73: its width now matches `fill_rect`'s
    /// width exactly, so the accent underline stops exactly where the
    /// gradient fill stops rather than always spanning the full row.
    accent_rect: egui::Rect,
    accent_left: egui::Color32,
    accent_right: egui::Color32,
}

/// Maps a row's `Class` to its share-bar hue (issue #44). `None` — either no
/// `Class` at all or `Class::Unknown` (which has no `Role`,
/// `Class::role`) — falls back to `SHARE_BAR_RGB_UNKNOWN`, the neutral grey.
fn share_bar_rgb(class: Option<Class>) -> (u8, u8, u8) {
    match class.and_then(|c| c.role()) {
        Some(Role::Healer) => SHARE_BAR_RGB_HEALER,
        Some(Role::Tank) => SHARE_BAR_RGB_TANK,
        Some(Role::Damage) => SHARE_BAR_RGB_DAMAGE,
        None => SHARE_BAR_RGB_UNKNOWN,
    }
}

/// Computes a row's damage-share bar fraction (issue #73): `damage` scaled
/// against `top_damage` — the highest-damage row in the current snapshot —
/// rather than against total encounter damage, so the top row's bar always
/// fills the full width regardless of how dominant it is over the rest of
/// the raid. Guards against a zero or negative `top_damage` (an empty or
/// degenerate snapshot) by returning `0.0` rather than dividing.
fn row_bar_frac(damage: i64, top_damage: i64) -> f32 {
    if top_damage <= 0 {
        0.0
    } else {
        damage as f32 / top_damage as f32
    }
}

/// Computes the two paints that make up a row's damage-share bar: a
/// `bar_frac`-scaled fill, vertically graded transparent -> `fill_bottom`,
/// and an accent line of the same width, horizontally graded `accent_left`
/// -> `accent_right` along its bottom edge (issue #73: the accent used to
/// always span the row's full width; it now matches the fill exactly, so
/// the underline stops where the gradient fill stops). `bar_frac` is a
/// 0.0-1.0 fraction — `draw_row` derives it from `row_bar_frac`, i.e. this
/// row's damage relative to the top row's, not this row's share of total
/// encounter damage. Both paints share the same role-derived hue
/// (`share_bar_rgb`, issue #44) and differ only in alpha. Pure geometry/
/// color math with no `egui::Ui` dependency, so it's unit-testable on its
/// own — `draw_row` just paints whatever it returns.
fn share_bar_paints(rect: egui::Rect, bar_frac: f32, class: Option<Class>) -> ShareBarPaints {
    let bar_frac = bar_frac.clamp(0.0, 1.0);
    let bar_width = rect.width() * bar_frac;

    let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(bar_width, rect.height()));

    let thickness = SHARE_BAR_ACCENT_THICKNESS.min(rect.height());
    let accent_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, rect.max.y - thickness),
        egui::vec2(bar_width, thickness),
    );

    let (r, g, b) = share_bar_rgb(class);
    let fill_bottom = egui::Color32::from_rgba_unmultiplied(r, g, b, SHARE_BAR_FILL_BOTTOM_ALPHA);
    let accent_left = egui::Color32::from_rgba_unmultiplied(r, g, b, SHARE_BAR_ACCENT_LEFT_ALPHA);
    let accent_right = egui::Color32::from_rgba_unmultiplied(r, g, b, 255);

    ShareBarPaints {
        fill_rect,
        fill_bottom,
        accent_rect,
        accent_left,
        accent_right,
    }
}

/// Square side of the per-row class icon (issue #9). Matches the source's
/// `Path 18x18`.
const ICON_SIZE: f32 = 18.0;

/// Gap on both sides of the icon: between the row's left edge and the icon,
/// and between the icon and the name that follows it. `3.5` so
/// `ICON_GUTTER_WIDTH` lands exactly on `25.0` — the source's 18px glyph
/// centered in a fixed 25px `SharedSizeGroup="p0"` column.
const ICON_MARGIN: f32 = 3.5;

/// Class icon tint (source `Fill="#ddd"`).
const CLASS_ICON_TINT: egui::Color32 = egui::Color32::from_rgb(0xDD, 0xDD, 0xDD);

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
/// default-size math and the row painter can never drift apart. Matches the
/// source's `Height="30"` exactly: `draw_rows` zeroes the vertical item
/// spacing for the row-list scope (decision 3), so rows are truly
/// contiguous — there is no separate inter-row gap to add on top of this.
const ROW_HEIGHT: f32 = 30.0;

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
const NAME_LEFT_PAD: f32 = 2.0;

/// Budgeted width for the name itself. `draw_row` paints names unclipped,
/// regular weight and proportional at `FONT_SIZE_ROW` (issues #56, #62) —
/// truncation/ellipsis is explicitly out of scope for issue #26 — so this is
/// not a hard cap, just enough room (roughly 20 proportional characters at
/// that size, more than the ~15 monospace ones it used to buy) that a
/// typical alphanumeric in-game name doesn't visually crowd the stat columns
/// that start right after it.
const NAME_WIDTH_BUDGET: f32 = 150.0;

/// Breathing room between the name budget and the first stat column.
const NAME_COLUMN_GAP: f32 = 10.0;

/// Right-edge margin, matching the `margin` `draw_rows` passes to
/// `column_anchors` for the rightmost column's anchor.
const COLUMN_RIGHT_MARGIN: f32 = 4.0;

/// Default opening height (issue #26; extended by issue #9 slice 2's title
/// line): the header's title line + timer/DPS/buttons row + separator + a
/// full 20-row raid roster, so no scrolling is needed on first launch. The
/// subtitle line is deliberately excluded — it is conditional (only
/// rendered once a scene name/id is known, `encounter_subtitle`), and the
/// default assumes it is absent.
///
/// Decision 3: `draw_rows` zeroes `item_spacing.y` for its own scope, so
/// rows are truly contiguous (`ROW_HEIGHT` is the full 30pt pitch, no
/// separate gap) and there is no gap between the separator and the first
/// row either. Only the two gaps *above* the row list — title->header and
/// header->separator — still pay `ITEM_SPACING_Y`.
///
///   title (20.0) + header row (22.0) + separator (6.0) + 20 rows * 30.0 (600.0)
///     + 2 gaps * 2.0 (4.0) = 652.0
fn default_inner_height() -> f32 {
    let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
    let gaps = 2.0 * ITEM_SPACING_Y;
    TITLE_LINE_HEIGHT + BUTTON_ROW_HEIGHT + SEPARATOR_HEIGHT + rows + gaps
}

/// Extra width folded into `default_inner_width` on top of the row-column
/// budget below, so the window opens wide enough to lay out the header's
/// stat row without wrapping. That row (issue #59's real rasterized pill
/// glyphs, the timer half-pill, and issue #62's 58pt inert status-toggle
/// cluster) is measured independently by `the_stat_pills_fit_the_default_
/// window_width`, which is the ground truth this headroom exists to
/// satisfy; 20pt clears the gap with room to spare across minor
/// font-metric variance between environments.
const HEADER_ROW_EXTRA_WIDTH: f32 = 20.0;

/// Default opening width (issue #26, widened for issue #9's icon gutter): a
/// name budget in front of the default stat columns' combined fixed width
/// (read out of `ColumnKind::spec` for whatever `Settings::default` enables,
/// never hardcoded), so names don't visually collide with them — plus the
/// fixed icon gutter now reserved at the row's left edge, so adding it
/// doesn't squeeze the name budget or the stat columns relative to before
/// issue #9 — plus `HEADER_ROW_EXTRA_WIDTH` so the header's own (now wider)
/// stat row fits too.
///
///   icon gutter (3.5 + 18.0 + 3.5 = 25.0) + left pad (2.0)
///     + name budget (150.0) + gap (10.0)
///     + columns (DPS 80.0 + crit 56.0 + lucky 56.0 + deaths 48.0 = 240.0)
///     + right margin (4.0) + header row headroom (20.0) = 451.0
///
/// The columns term grew with issue #49's death column joining the default
/// set; because it is summed rather than written down, the default window
/// widened to keep the same name budget instead of quietly squeezing it.
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
        + HEADER_ROW_EXTRA_WIDTH
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
        .with_min_inner_size(MIN_INNER_SIZE);
    if let Some(position) = window_position {
        builder = builder.with_position(position);
    }
    builder
}

/// Panel fill: translucent near-black, the overlay's own value rather than the
/// source's `WindowData.DefaultBackgroundColor` `#232830` @ 0.5. That slate
/// grey reads as washed-out over game footage; the original ShinraMeter
/// silhouette is near-black, so we keep `#121216` at 200/255 here. Fixed
/// constants deliberately — the source binds all three of these to a settings
/// VM, and user-configurable chrome is out of scope for now.
const PANEL_FILL: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(18, 18, 22, 200);
/// Panel border: `DefaultBorderColor` `#717b85`, at the same 0.5 opacity the
/// source applies to the whole Border (fill and stroke alike).
const PANEL_BORDER_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x71, 0x7b, 0x85, 128);
/// `TopmostBorderStyle`'s `BorderThickness="1"`.
const PANEL_BORDER_WIDTH: f32 = 1.0;
/// `TopmostBorderStyle`'s `CornerRadius="8"`.
const PANEL_CORNER_RADIUS: u8 = 8;
/// Height of `draw_header`'s stat-pill / window-control row — the source's
/// `Height="22"` stat pills. Named because `apply_theme` installs it as
/// `interact_size.y` *and* `default_inner_height` budgets for it; reading
/// `egui::Style::default()` there (as this used to) silently used egui's 18.0
/// instead of the themed value.
const BUTTON_ROW_HEIGHT: f32 = 22.0;

/// Dark, compact visuals with monospace numerals for the overlay.
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    // The `Frame` `OverlayApp::ui` wraps the `CentralPanel` in now owns the
    // fill (`PANEL_FILL`) — leaving this non-transparent would double-paint
    // it.
    visuals.panel_fill = egui::Color32::TRANSPARENT;
    // The source's `DarkBarColor` popup background, used by the settings
    // menu (not the same as the panel fill above).
    visuals.window_fill = egui::Color32::from_rgb(0x11, 0x11, 0x17);
    visuals.window_corner_radius = egui::CornerRadius::same(PANEL_CORNER_RADIUS);
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 2.0);
        style.spacing.interact_size.y = BUTTON_ROW_HEIGHT;
        // `TOOLBAR_ICON_SIZE (14) + 2*4 == BUTTON_ROW_HEIGHT (22)`.
        style.spacing.button_padding = egui::vec2(4.0, 4.0);
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

    // -- issue #68: gesture pointer sourced from the OS cursor, not the
    // window being dragged --------------------------------------------------

    #[test]
    fn gesture_pointer_a_moving_window_cannot_drag_the_os_cursor_along_with_it() {
        // The bug: `window.min + local` re-derives screen position from the
        // very window the gesture just moved, so the reconstructed pointer
        // drifts with the window even though the real cursor hasn't moved.
        // With an OS-supplied cursor, two different window rects (as if the
        // window moved between frames) must yield the identical screen
        // position — proving the measured pointer can no longer be dragged
        // along with the window, which is exactly what made `delta` grow
        // every frame in the runaway.
        let os_cursor = egui::pos2(500.0, 400.0);
        let local = egui::pos2(50.0, 50.0);
        let window_before =
            egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(300.0, 200.0));
        let window_after =
            egui::Rect::from_min_size(egui::pos2(140.0, 130.0), egui::vec2(300.0, 200.0));

        let before = gesture_pointer(Some(os_cursor), window_before, local);
        let after = gesture_pointer(Some(os_cursor), window_after, local);

        assert_eq!(before, os_cursor);
        assert_eq!(after, os_cursor);
        assert_eq!(before, after);
    }

    #[test]
    fn gesture_pointer_falls_back_to_the_window_origin_without_an_os_cursor() {
        // Non-Windows dev hosts have no `GetCursorPos` equivalent wired up
        // (`platform::cursor_position` returns `None` there), so this
        // fallback just preserves the previous, pre-#68 behaviour on those
        // dev-only hosts.
        let window = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(300.0, 200.0));
        let local = egui::pos2(50.0, 50.0);

        assert_eq!(
            gesture_pointer(None, window, local),
            egui::pos2(150.0, 150.0)
        );
    }

    // -- header restyle (top-bar restyle: hamburger removal, total-damage
    // stat, title separator, icon tint) ------------------------------------

    /// Builds a minimal `Snapshot` for header-rendering tests: a resolved
    /// boss name (so `encounter_title` returns non-empty text) and a
    /// distinctive `total_damage` so the formatted figure is unambiguous in
    /// assertions below.
    fn header_test_snapshot(total_damage: i64) -> Snapshot {
        Snapshot {
            duration_ms: 90_000,
            total_damage,
            total_dps: 12_345.0,
            rows: Vec::new(),
            encounter: EncounterInfo {
                boss_monster_id: Some(1),
                is_boss: true,
                boss_name: Some("Bahaar"),
                scene_id: None,
                scene_name: None,
            },
        }
    }

    /// Walks a painted `Shape`, collecting the text of every `Shape::Text`
    /// found — recursing into `Shape::Vec` since egui groups a layout's
    /// child shapes (e.g. `ui.horizontal`'s row) that way. `Galley`
    /// dereferences to `str` (`Deref<Target = str>`), so `galley.text()`
    /// hands back exactly the string that was laid out.
    fn collect_text_shapes(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text_shape) => out.push(text_shape.galley.text().to_string()),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_text_shapes(s, out);
                }
            }
            _ => {}
        }
    }

    /// Walks a painted `Shape`, collecting every `Shape::Mesh`'s
    /// `(texture_id, tint)` — `Painter::image` bakes its `tint` directly
    /// into every vertex (`Mesh::add_rect_with_uv`), so a mesh's first
    /// vertex color is exactly the tint the blit was painted with.
    fn collect_image_texture_tints(
        shape: &egui::Shape,
        out: &mut Vec<(egui::TextureId, egui::Color32)>,
    ) {
        match shape {
            egui::Shape::Mesh(mesh) => {
                if let Some(vertex) = mesh.vertices.first() {
                    out.push((mesh.texture_id, vertex.color));
                }
            }
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_image_texture_tints(s, out);
                }
            }
            _ => {}
        }
    }

    /// Renders `draw_header` and returns the text of every string it
    /// painted this frame (title, subtitle, and every `ui.label` in the
    /// button row) by walking the frame's raw `FullOutput::shapes` — the
    /// title/subtitle are painted directly via `ui.painter().text`, which
    /// never reaches accesskit, so this reads the same ground truth for
    /// both painter-drawn and widget-drawn text instead of two different
    /// mechanisms.
    fn header_rendered_texts(snapshot: &Snapshot) -> Vec<String> {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_header(
                ui,
                &ctx,
                snapshot,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                ChromeHandle {
                    gesture: &mut WindowGesture::default(),
                    collapse: &mut CollapseState::default(),
                },
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();
        texts
    }

    /// One header frame's painted geometry: every text, image blit and
    /// filled rect, each already intersected with the clip rect it was
    /// painted under. Issue #75: without a screenshot of the Windows build,
    /// measuring the boxes a frame really painted is the only way to check
    /// what the header looks like — `collect_text_shapes` throws the
    /// geometry away, and the pure-geometry pill tests only prove
    /// `pill_size` and `pill_content_layout` agree with *each other*, not
    /// that the paint agrees with them or that nothing else lands on top.
    struct HeaderFrame {
        texts: Vec<(String, egui::Rect)>,
        images: Vec<(egui::TextureId, egui::Rect)>,
        rects: Vec<(egui::Color32, egui::Rect)>,
        glyphs: Vec<(GlyphIcon, egui::TextureId)>,
    }

    impl HeaderFrame {
        /// The union of every text shape painted for `value` — plural
        /// because the faux-bold pass paints the same string twice, and both
        /// passes together are what the eye sees.
        fn text_box(&self, value: &str) -> egui::Rect {
            self.texts
                .iter()
                .filter(|(painted, _)| painted == value)
                .map(|(_, rect)| *rect)
                .reduce(egui::Rect::union)
                .unwrap_or_else(|| panic!("the header never painted {value:?}: {:?}", self.texts))
        }

        /// Every *visible* box `glyph` was blitted into — blits clipped away
        /// entirely are dropped, since an empty intersection is exactly what
        /// "this never reached the screen" looks like.
        fn glyph_boxes(&self, glyph: GlyphIcon) -> Vec<egui::Rect> {
            let id = self
                .glyphs
                .iter()
                .find(|(painted, _)| *painted == glyph)
                .map(|(_, id)| *id)
                .unwrap_or_else(|| panic!("{glyph:?} has no texture"));
            self.images
                .iter()
                .filter(|(painted, rect)| *painted == id && rect.is_positive())
                .map(|(_, rect)| *rect)
                .collect()
        }

        /// The box of the (single) rect filled with `fill` — how a pill's
        /// own chrome is identified, since each pill kind has its own fill.
        fn fill_box(&self, fill: egui::Color32) -> egui::Rect {
            self.rects
                .iter()
                .find(|(painted, _)| *painted == fill)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("the header never filled a {fill:?} rect"))
        }
    }

    /// Walks a painted `Shape`, collecting the rect of every text, image and
    /// filled rect in it, clipped to `clip` — the clip rect is half the
    /// geometry for anything painted through `Painter::with_clip_rect`, so
    /// ignoring it would measure boxes the screen never shows.
    fn collect_painted_boxes(shape: &egui::Shape, clip: egui::Rect, frame: &mut HeaderFrame) {
        match shape {
            egui::Shape::Text(text) => frame.texts.push((
                text.galley.text().to_string(),
                egui::Rect::from_min_size(text.pos, text.galley.size()).intersect(clip),
            )),
            egui::Shape::Mesh(mesh) => frame
                .images
                .push((mesh.texture_id, mesh.calc_bounds().intersect(clip))),
            egui::Shape::Rect(rect) => frame.rects.push((rect.fill, rect.rect.intersect(clip))),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_painted_boxes(s, clip, frame);
                }
            }
            _ => {}
        }
    }

    /// Renders `draw_header` at the app's real default window size and hands
    /// back everything it painted. The size matters: the header's decoration
    /// is positioned off the panel's own edges, so the unbounded width a
    /// bare `RawInput` implies would put it where the app never paints it.
    fn header_painted_boxes(snapshot: &Snapshot) -> HeaderFrame {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let glyphs: Vec<(GlyphIcon, egui::TextureId)> = [
            GlyphIcon::Emblem,
            GlyphIcon::Timer,
            GlyphIcon::Speed,
            GlyphIcon::Heart,
        ]
        .into_iter()
        .filter_map(|glyph| icons.glyphs.get(glyph).map(|texture| (glyph, texture.id())))
        .collect();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(default_inner_width(), default_inner_height()),
            )),
            ..Default::default()
        };

        let mut frame = HeaderFrame {
            texts: Vec::new(),
            images: Vec::new(),
            rects: Vec::new(),
            glyphs,
        };
        let output = ctx.run_ui(input, |ui| {
            draw_header(
                ui,
                &ctx,
                snapshot,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                ChromeHandle {
                    gesture: &mut WindowGesture::default(),
                    collapse: &mut CollapseState::default(),
                },
            );
        });
        for clipped in &output.shapes {
            collect_painted_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        output.drop_without_applying_deltas();
        frame
    }

    /// Issue #75: the timer half-pill's clock glyph must not overlap the
    /// duration it leads, measured on the shapes `draw_header` *actually
    /// paints*, and both must sit inside the pill's own filled box.
    #[test]
    fn the_timer_pills_clock_glyph_never_overlaps_its_value() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);

        let pill = frame.fill_box(TIMER_PILL_FILL);
        let value = frame.text_box(&fmt_duration(snapshot.duration_ms));
        let glyphs = frame.glyph_boxes(GlyphIcon::Timer);
        assert_eq!(
            glyphs.len(),
            1,
            "expected exactly one clock blit: {glyphs:?}"
        );
        let glyph = glyphs[0];

        let gap = value.left() - glyph.right();
        assert!(
            gap >= PILL_ICON_GAP - 0.01,
            "the clock glyph {glyph:?} and the value {value:?} are only {gap}pt apart \
             (want {PILL_ICON_GAP}) inside pill {pill:?}"
        );
        for (what, rect) in [("glyph", glyph), ("value", value)] {
            assert!(
                pill.expand(0.01).contains_rect(rect),
                "the timer pill's {what} {rect:?} spills out of the pill box {pill:?}"
            );
        }
    }

    /// Issue #75: the gutter emblem is decoration for the header's *text*
    /// rows — the ones `header_text_rect` indents by `HEADER_GUTTER_WIDTH`
    /// to make room for it. The stat-pill row has no such gutter: the timer
    /// half-pill is deliberately flush against the panel's left border, in
    /// exactly the column the emblem occupies. Since the emblem's box is
    /// 60pt tall against a 20pt title row, only its clip keeps its ink out
    /// of that row — and it must, or it paints straight through the pill,
    /// under a 6%-alpha fill, behind a clock glyph tinted at a third alpha.
    #[test]
    fn the_gutter_emblem_never_reaches_the_stat_pill_row() {
        let frame = header_painted_boxes(&header_test_snapshot(30_100_000_000));
        let pill = frame.fill_box(TIMER_PILL_FILL);

        for emblem in frame.glyph_boxes(GlyphIcon::Emblem) {
            assert!(
                !emblem.intersects(pill),
                "emblem ink {emblem:?} lands on the timer pill {pill:?}"
            );
        }
    }

    /// The stray `☰` hamburger label had no counterpart in the reference
    /// render and no behavior of its own (the whole header band is already
    /// the drag surface) — it must not appear anywhere in the rendered
    /// header.
    #[test]
    fn draw_header_omits_hamburger_glyph() {
        let texts = header_rendered_texts(&header_test_snapshot(30_100_000_000));
        assert!(!texts.iter().any(|text| text == "☰"));
    }

    /// Issue #71: the window-control icon-button cluster (Close/Minimize/
    /// Reset/Settings) no longer renders directly in the header's stat row
    /// — those actions moved into the chevron's dropdown menu, which paints
    /// nothing until opened. A closed-menu frame must carry no accessible
    /// node labeled for any of the old buttons, only the chevron's own
    /// "Menu" label.
    #[test]
    fn draw_header_no_longer_renders_the_window_control_cluster() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(30_100_000_000);

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
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
                ChromeHandle {
                    gesture: &mut WindowGesture::default(),
                    collapse: &mut CollapseState::default(),
                },
            );
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let labels: Vec<String> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(str::to_string))
            .collect();
        output.drop_without_applying_deltas();

        for stale in ["Close", "Reset", "Settings", "Minimize"] {
            assert!(
                !labels.iter().any(|l| l == stale),
                "stale window-control label {stale:?} still renders directly in the header: {labels:?}"
            );
        }
        assert!(
            labels.iter().any(|l| l == "Menu"),
            "expected the chevron's own \"Menu\" label, got {labels:?}"
        );
    }

    /// The reference render shows a total-damage figure alongside the DPS
    /// figure (e.g. "30.1B"), abbreviated with the same `fmt_short` used
    /// everywhere else — `snapshot.total_damage` existed but was never
    /// painted before this change.
    #[test]
    fn draw_header_shows_total_damage_abbreviated() {
        let texts = header_rendered_texts(&header_test_snapshot(30_100_000_000));
        let expected = fmt_short(30_100_000_000);
        assert_eq!(expected, "30.1B");
        assert!(
            texts.iter().any(|text| text.contains(&expected)),
            "expected a painted text containing {expected:?}, got {texts:?}"
        );
    }

    /// The header band's height budget (`header_band_height`) must still
    /// cover everything `draw_header` actually paints even after adding the
    /// total-damage stat to the button row and the fading separator under
    /// the title — neither should make the rendered content taller than the
    /// band `draw_header` already computes as its drag surface.
    #[test]
    fn draw_header_fits_within_its_own_band_height() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        let mut rendered_height = 0.0;
        let mut interact_size_y = 0.0;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
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
                ChromeHandle {
                    gesture: &mut WindowGesture::default(),
                    collapse: &mut CollapseState::default(),
                },
            );
            interact_size_y = ui.spacing().interact_size.y;
            rendered_height = ui.min_rect().height();
        });
        output.drop_without_applying_deltas();

        let has_subtitle = encounter_subtitle(&snapshot.encounter).is_some();
        let band = header_band_height(has_subtitle, interact_size_y);
        assert!(
            rendered_height <= band,
            "rendered header ({rendered_height}) overflowed its band ({band})"
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

    // -- typography scale (issue #62) --------------------------------------

    /// The scale is pulled straight from `mvvm_refactor_wip`'s XAML, not
    /// re-derived from the render, so this pins the numbers rather than an
    /// ordering — the source's row scale is deliberately flat (`FONT_SIZE_ROW
    /// == FONT_SIZE_COUNTER`), so there is no "largest to smallest" chain
    /// left to assert.
    #[test]
    fn font_scale_matches_the_source_metrics() {
        // Walked as a slice rather than asserted pair by pair: comparing a
        // constant directly to a literal is fine at runtime, but doing it
        // one-by-one for five constants is what this loop avoids repeating.
        let scale = [
            FONT_SIZE_TIMER,
            FONT_SIZE_TITLE,
            FONT_SIZE_ROW,
            FONT_SIZE_PILL_VALUE,
            FONT_SIZE_SUBTITLE,
        ];
        assert_eq!(scale, [16.0, 13.0, 13.0, 12.0, 10.0]);
        assert_eq!(FONT_SIZE_COUNTER, FONT_SIZE_ROW);
    }

    /// `regular` is always the plain proportional family.
    #[test]
    fn regular_is_the_proportional_family() {
        assert_eq!(regular(12.0).family, egui::FontFamily::Proportional);
        assert_eq!(regular(12.0).size, 12.0);
    }

    /// Without a real bold installed — every unit test, and every Linux run
    /// — `bold` must hand back the plain proportional font rather than the
    /// named `"bold"` family, which is unbound on a bare `egui::Context` and
    /// would make epaint panic on the first paint.
    #[test]
    fn bold_degrades_to_proportional_when_no_real_bold_is_installed() {
        assert!(!fonts::has_real_bold());
        assert_eq!(bold(12.0), regular(12.0));
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

    /// Per-column emphasis (issue #62): the source's row scale is flat, so
    /// every metric column — DPS included — shares `FONT_SIZE_ROW`. `Dps` is
    /// still its own `ColumnEmphasis::Value` (a distinct level for future
    /// hooks), it just no longer maps to a different font.
    #[test]
    fn every_metric_column_shares_the_source_row_metric() {
        assert_eq!(column_emphasis(ColumnKind::Dps), ColumnEmphasis::Value);
        assert!(!ColumnEmphasis::Value.is_pill());
        for kind in ColumnKind::ALL {
            assert_eq!(
                column_emphasis(kind).font().size,
                FONT_SIZE_ROW,
                "{kind:?} should share the row's flat metric size"
            );
        }
    }

    /// Percentage columns are distinguished by color, not size — the source
    /// carries no separate `FontSize` for them.
    #[test]
    fn percentage_columns_are_distinguished_by_color_not_size() {
        for kind in [
            ColumnKind::SharePct,
            ColumnKind::CritPct,
            ColumnKind::LuckyPct,
        ] {
            assert_eq!(column_emphasis(kind), ColumnEmphasis::Percent);
        }
        assert_ne!(
            ColumnKind::CritPct.spec().color,
            ColumnKind::Dps.spec().color
        );
    }

    // -- stat pills (issue #56, #59, #62) ----------------------------------

    /// A pill is padding + text + gap + icon, and nothing else — the
    /// formula issue #49's counter pill inherits.
    #[test]
    fn pill_width_is_padding_plus_text_plus_gap_plus_icon() {
        let text = egui::vec2(40.0, 15.0);
        let size = pill_size(text, 14.0, 22.0);
        assert_eq!(size.x, 2.0 * PILL_PAD_X + text.x + PILL_ICON_GAP + 14.0);
    }

    /// The height cap is what keeps the pills from silently growing
    /// `draw_header`'s band past the drag surface it registered: text plus
    /// padding is taller than the button row at the header's pill size, so
    /// this clamp is load-bearing, not theoretical.
    #[test]
    fn pill_height_never_exceeds_the_row_it_sits_in() {
        for text_height in [10.0, 15.0, 40.0] {
            let size = pill_size(egui::vec2(30.0, text_height), 14.0, BUTTON_ROW_HEIGHT);
            assert!(
                size.y <= BUTTON_ROW_HEIGHT,
                "a {text_height}pt text grew the pill to {}pt",
                size.y
            );
        }
    }

    /// A short text still gets a pill shorter than the cap — the clamp is a
    /// ceiling, not a fixed height.
    #[test]
    fn pill_height_follows_its_text_below_the_cap() {
        let size = pill_size(egui::vec2(30.0, 10.0), 14.0, 18.0);
        assert_eq!(size.y, 10.0 + 2.0 * PILL_PAD_Y);
    }

    /// Header layout: value first, icon after it, both inside the padding.
    #[test]
    fn pill_content_sits_inside_its_padding_with_the_icon_trailing() {
        let text = egui::vec2(40.0, 15.0);
        let size = pill_size(text, 14.0, 18.0);
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), size);
        let (text_pos, icon_rect) = pill_content_layout(rect, text, 14.0, false);

        assert_eq!(text_pos.x, rect.left() + PILL_PAD_X);
        assert_eq!(text_pos.y, rect.center().y);
        assert!(icon_rect.left() >= text_pos.x + text.x);
        assert!(
            (icon_rect.right() - (rect.right() - PILL_PAD_X)).abs() < 0.01,
            "icon should end exactly one padding short of the pill's right edge"
        );
        assert_eq!(icon_rect.center().y, rect.center().y);
    }

    /// `icon_first` swaps the two without changing the pill's width — the
    /// ordering issue #49's skull-then-count counter (and the timer
    /// half-pill) need.
    #[test]
    fn pill_content_can_lead_with_its_icon() {
        let text = egui::vec2(40.0, 15.0);
        let size = pill_size(text, 14.0, 18.0);
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size);
        let (text_pos, icon_rect) = pill_content_layout(rect, text, 14.0, true);

        assert_eq!(icon_rect.left(), rect.left() + PILL_PAD_X);
        assert!(text_pos.x >= icon_rect.right());
        assert!(
            (rect.right() - PILL_PAD_X - (text_pos.x + text.x)).abs() < 0.01,
            "the value should end exactly one padding short of the right edge"
        );
    }

    /// Issue #75: the header's pills at their *production* parameters — real
    /// font sizes, real `PILL_*` constants, real `interact_size.y` cap — must
    /// lay their glyph and their value out disjointly, both fully inside the
    /// pill box, with the pill exactly as wide as the sum of its parts. This
    /// is the numeric stand-in for the screenshot we cannot take of the
    /// Windows build: every "misplaced / overlapping" symptom the header can
    /// have shows up as one of these four invariants failing.
    #[test]
    fn the_header_pills_lay_glyph_and_value_out_inside_their_box() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        // Lay a frame out first so the real (non-empty) fonts are loaded.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        for (name, pill) in [
            ("timer", StatPill::timer("02:39", None)),
            ("dps", StatPill::header("1000.0K/s", None)),
            ("damage", StatPill::header("30.1B", None)),
        ] {
            let text = ctx.fonts_mut(|f| {
                f.layout_no_wrap(pill.value.to_owned(), bold(pill.size), pill.value_color)
                    .rect
                    .size()
            });
            let size = pill_size(text, pill.icon_side, BUTTON_ROW_HEIGHT);
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size);
            let (text_pos, icon_rect) =
                pill_content_layout(rect, text, pill.icon_side, pill.icon_first);
            // `paint_bold_text` anchors LEFT_CENTER, so this is the box the
            // value actually covers.
            let text_rect =
                egui::Rect::from_min_size(egui::pos2(text_pos.x, text_pos.y - text.y / 2.0), text);

            assert!(
                (size.x - (2.0 * PILL_PAD_X + text.x + PILL_ICON_GAP + pill.icon_side)).abs()
                    < 0.01,
                "{name}: pill width {} is not padding + text {} + gap + icon",
                size.x,
                text.x
            );
            let gap = if pill.icon_first {
                text_rect.left() - icon_rect.right()
            } else {
                icon_rect.left() - text_rect.right()
            };
            assert!(
                gap >= PILL_ICON_GAP - 0.01,
                "{name}: glyph and value are only {gap}pt apart (want {PILL_ICON_GAP})"
            );
            assert!(
                icon_rect.left() >= rect.left() - 0.01
                    && icon_rect.right() <= rect.right() + 0.01
                    && icon_rect.top() >= rect.top() - 0.01
                    && icon_rect.bottom() <= rect.bottom() + 0.01,
                "{name}: glyph box {icon_rect:?} spills out of the pill {rect:?}"
            );
            assert!(
                text_rect.left() >= rect.left() - 0.01
                    && text_rect.right() <= rect.right() + 0.01
                    && text_rect.top() >= rect.top() - 0.01
                    && text_rect.bottom() <= rect.bottom() + 0.01,
                "{name}: value box {text_rect:?} (text {text:?}) spills out of the pill {rect:?}"
            );
        }
    }

    /// The DPS/damage pills are full ovals (corner radius at least half the
    /// button row height, uniform on all four corners); the timer half-pill
    /// is flush against the panel's left border — the source's
    /// `CornerRadius="0 13 13 0"`.
    #[test]
    fn header_pills_are_ovals_and_the_timer_is_a_half_pill() {
        let header = StatPill::header("1", None);
        assert_eq!(header.corner_radius.nw, header.corner_radius.ne);
        assert_eq!(header.corner_radius.ne, header.corner_radius.se);
        assert_eq!(header.corner_radius.se, header.corner_radius.sw);
        assert!(header.corner_radius.nw as f32 >= BUTTON_ROW_HEIGHT / 2.0 - 1.0);

        let timer = StatPill::timer("1", None);
        assert_eq!(timer.corner_radius.nw, 0);
        assert_eq!(timer.corner_radius.sw, 0);
        assert!(timer.corner_radius.ne > 0);
        assert!(timer.corner_radius.se > 0);
    }

    /// `StatPill::timer`'s corner radius has zero west corners — flush
    /// against the panel's left border — independent of the general oval
    /// check above.
    #[test]
    fn the_timer_pill_is_flush_against_the_left_border() {
        let pill = StatPill::timer("1", None);
        assert_eq!(pill.corner_radius.nw, 0);
        assert_eq!(pill.corner_radius.sw, 0);
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
            measure("1000.0K/s", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        let dmg = pill_size(
            measure("1000.0B", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        // The three inert status toggles (decision 5): a fixed-width pill,
        // not measured text.
        let toggles = 2.0 * TOGGLE_PAD_X
            + TOGGLE_MOUSE_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLOUD_SIDE
            + TOGGLE_GAP
            + TOGGLE_QUEUE_SIDE;

        // Three gaps between the outer horizontal's four direct children
        // (timer, dps, dmg, toggle cluster).
        let gaps = 3.0 * 6.0;

        let total = timer + dps + dmg + toggles + gaps;
        assert!(
            total <= default_inner_width(),
            "stat row needs {total}pt but the default window is only {}pt wide",
            default_inner_width()
        );
    }

    /// The source's `OffBrush="#1fff"`: every glyph the cluster blits — the
    /// mouse-off LED, the cloud-off LED, and the queue gauge's check glyph —
    /// is tinted `TOGGLE_OFF_COLOR`, never an "on" color, since none of
    /// those features exist (decision 5).
    #[test]
    fn the_status_toggles_render_in_their_off_state() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let mouse_off = icons.glyphs.get(GlyphIcon::MouseOff).unwrap().id();
        let cloud_off = icons.glyphs.get(GlyphIcon::CloudOff).unwrap().id();
        let check = icons.glyphs.get(GlyphIcon::Check).unwrap().id();

        let mut blits = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &icons);
        });
        for clipped in &output.shapes {
            collect_image_texture_tints(&clipped.shape, &mut blits);
        }
        output.drop_without_applying_deltas();

        for expected in [mouse_off, cloud_off, check] {
            let tint = blits
                .iter()
                .find(|(id, _)| *id == expected)
                .map(|(_, c)| *c);
            assert_eq!(
                tint,
                Some(TOGGLE_OFF_COLOR),
                "{expected:?} was not blitted at TOGGLE_OFF_COLOR: {blits:?}"
            );
        }
    }

    /// The three status toggles are strictly non-interactive (decision 5,
    /// issue #62): no click handling, no hover cursor, no tooltip that
    /// implies they work. `widget_info` is never called for them, so no
    /// accesskit node in the tree may claim a `Button` role.
    #[test]
    fn the_status_toggles_are_inert() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &icons);
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        output.drop_without_applying_deltas();

        assert!(
            update
                .nodes
                .iter()
                .all(|(_, node)| node.role() != egui::accesskit::Role::Button),
            "the inert status toggles must never expose a Button role"
        );
    }

    /// The reference drops the trailing " DPS"/" DMG" words in favor of the
    /// `/s` suffix and the pill icons — the words are pure width.
    #[test]
    fn draw_header_drops_the_dps_and_dmg_words() {
        let texts = header_rendered_texts(&header_test_snapshot(30_100_000_000));
        assert!(
            !texts.iter().any(|text| text.contains(" DPS")),
            "header still paints a \" DPS\" word: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains(" DMG")),
            "header still paints a \" DMG\" word: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.ends_with("/s")),
            "header should show the party DPS with a /s suffix: {texts:?}"
        );
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
            deaths: 0,
            ability_score,
            season_strength: None,
        }
    }

    fn sample_season_row(season_strength: Option<u32>) -> PlayerRow {
        PlayerRow {
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
        // A value that `fmt_short` would abbreviate to "12.3M" — the full
        // digit string must be rendered instead (owner requirement: ability
        // score and season strength always show the complete figure).
        let row = sample_row(Some(12_345_678));
        let column = ColumnKind::AbilityScore.spec();
        assert_eq!((column.text)(&row), "12345678");
    }

    #[test]
    fn season_strength_column_blank_when_none() {
        let row = sample_season_row(None);
        let column = ColumnKind::SeasonStrength.spec();
        assert_eq!((column.text)(&row), "");
    }

    #[test]
    fn season_strength_column_formats_value_when_some() {
        // Same full-digit requirement as ability score above.
        let row = sample_season_row(Some(12_345_678));
        let column = ColumnKind::SeasonStrength.spec();
        assert_eq!((column.text)(&row), "12345678");
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

    /// `TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT` is the
    /// source's `Height="36"` header grid — pinned as a sum, not three
    /// separate literals, so a future edit to any one constant can't drift
    /// from the source without this test catching it.
    #[test]
    fn the_title_and_subtitle_lines_add_up_to_the_source_header_grid() {
        let total = TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT;
        assert_eq!(total, 36.0);
    }

    /// The emblem is bled off the title row's left edge and is taller than
    /// the text band it decorates in both directions, so it only ever shows
    /// through a clip. Rewritten for issue #75: it used to assert the box
    /// merely started above the row (the hardcoded `-8` of the
    /// subtitle-present case), which said nothing about the far bigger
    /// overhang *below* — the one that was painting over the stat pills.
    #[test]
    fn the_header_emblem_bleeds_off_the_left_edge_and_out_of_the_text_band() {
        let row = egui::Rect::from_min_size(egui::pos2(10.0, 40.0), egui::vec2(380.0, 20.0));
        for has_subtitle in [false, true] {
            let band = header_text_band_height(has_subtitle);
            let rect = header_emblem_rect(row, band);
            assert!(rect.left() < row.left());
            assert!(rect.top() < row.top());
            assert!(rect.bottom() > row.top() + band);
            assert_eq!(rect.width(), HEADER_EMBLEM_SIZE);
            assert_eq!(rect.height(), HEADER_EMBLEM_SIZE);
        }
    }

    /// The source's centering, reproduced exactly for the case it was read
    /// off (`Height="36"` grid, `Margin="… -8"`): a top edge 8pt above the
    /// grid. With no subtitle line the grid is only `TITLE_LINE_HEIGHT`
    /// tall, and the same formula re-centers on it rather than leaving the
    /// art hanging low (issue #75).
    #[test]
    fn the_header_emblem_is_centered_on_the_text_band_it_decorates() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(380.0, 20.0));
        assert_eq!(header_emblem_rect(row, 36.0).top(), -8.0);
        for has_subtitle in [false, true] {
            let band = header_text_band_height(has_subtitle);
            let rect = header_emblem_rect(row, band);
            let above = row.top() - rect.top();
            let below = rect.bottom() - (row.top() + band);
            assert!(
                (below - (above + HEADER_EMBLEM_BOTTOM_BLEED)).abs() < 0.01,
                "the negative bottom margin should hang the emblem \
                 {HEADER_EMBLEM_BOTTOM_BLEED}pt further below the band than above it \
                 (has_subtitle={has_subtitle}): above {above}, below {below}"
            );
        }
    }

    /// The band the drag surface covers is the text rows plus one gap plus
    /// the stat-pill row — the emblem's clip (the text rows) is strictly
    /// shorter, so confining the emblem to it cannot have moved the drag
    /// surface or the collapsed window's height.
    #[test]
    fn the_text_band_is_the_drag_band_minus_the_stat_pill_row() {
        for has_subtitle in [false, true] {
            let text = header_text_band_height(has_subtitle);
            let band = header_band_height(has_subtitle, BUTTON_ROW_HEIGHT);
            assert_eq!(band, text + ITEM_SPACING_Y + BUTTON_ROW_HEIGHT);
            assert!(text < band);
        }
        assert_eq!(header_text_band_height(true), 36.0);
    }

    // -- header background wash (issue #59, #62) ------------------------

    /// A stand-in central-panel rect for the wash geometry tests — wider and
    /// far taller than the wash itself, like the real panel.
    fn wash_test_panel() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(12.0, 30.0), egui::vec2(400.0, 300.0))
    }

    /// The wash is inset from the panel on both sides and its top so its
    /// square corners stay inside the panel's rounded border, and it runs to
    /// its own fixed height rather than the panel's — it is decoration behind
    /// the header rows, not a full-height background.
    #[test]
    fn the_header_wash_is_inset_from_the_panel_and_keeps_its_own_fixed_height() {
        let panel = wash_test_panel();
        let wash = header_wash_rect(panel);
        assert_eq!(wash.left() - panel.left(), HEADER_WASH_INSET);
        assert_eq!(panel.right() - wash.right(), HEADER_WASH_INSET);
        assert_eq!(wash.top() - panel.top(), HEADER_WASH_INSET);
        assert_eq!(wash.width(), panel.width() - 2.0 * HEADER_WASH_INSET);
        assert_eq!(wash.height(), HEADER_WASH_HEIGHT);
        assert!(wash.bottom() < panel.bottom());
    }

    /// The wash emblem hangs off the wash's right edge by exactly the
    /// source's `-25` right margin, and is vertically centered on the wash —
    /// the mirror of the gutter emblem's left-edge bleed, and the reason the
    /// wash must be painted through its own clip rect.
    #[test]
    fn the_wash_emblem_bleeds_off_the_right_edge_by_the_named_overhang() {
        let wash = header_wash_rect(wash_test_panel());
        let emblem = header_wash_emblem_rect(wash);
        assert_eq!(emblem.right() - wash.right(), HEADER_WASH_EMBLEM_BLEED);
        assert!(emblem.left() > wash.left());
        assert_eq!(emblem.center().y, wash.center().y);
        assert_eq!(emblem.width(), HEADER_WASH_EMBLEM_SIZE);
        assert_eq!(emblem.height(), HEADER_WASH_EMBLEM_SIZE);
    }

    /// The wash emblem is drawn far larger than the band it decorates, so it
    /// overflows the wash vertically too — a change that made it fit would
    /// mean it had stopped reading as an oversized watermark.
    #[test]
    fn the_wash_emblem_is_taller_than_the_wash_it_sits_in() {
        let wash = header_wash_rect(wash_test_panel());
        let emblem = header_wash_emblem_rect(wash);
        assert!(emblem.height() > wash.height());
        assert!(emblem.top() < wash.top());
        assert!(emblem.bottom() > wash.bottom());
    }

    // -- column_anchors (issue #8) --------------------------------------

    /// A stand-in three-column layout (same widths the old fixed
    /// `STAT_COLUMNS` array used) for tests that exercise `column_anchors`'
    /// pure math and don't care where the widths came from.
    const TEST_COLUMNS: [StatColumn; 3] = [
        StatColumn {
            width: 56.0,
            text: |row| fmt_short(row.damage),
            color: egui::Color32::WHITE,
        },
        StatColumn {
            width: 56.0,
            text: |row| format!("{}/s", fmt_short(row.dps as i64)),
            color: egui::Color32::WHITE,
        },
        StatColumn {
            width: 44.0,
            text: |row| fmt_share(row.share_pct),
            color: egui::Color32::WHITE,
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

    // -- column_clip_rect (stat text can overflow its fixed column width) --

    /// A narrow row (`MIN_INNER_SIZE` is 220x90 and up to
    /// `ColumnKind::ALL.len()` columns can be enabled, so this is reachable
    /// in normal use) makes `column_anchors` scale the gap between adjacent
    /// anchors below the columns' nominal widths. Clipping to that scaled
    /// gap would hide the *leading* characters of perfectly in-range text —
    /// right-aligned text is clipped from the left, so a truncated number
    /// reads as a smaller one. Every column's slot must therefore admit its
    /// full nominal `width` no matter how compressed the row is, so that a
    /// value already known to fit its budget
    /// (`widest_formatted_text_fits_its_column_width_budget`) is never cut.
    #[test]
    fn column_clip_rect_admits_the_full_column_budget_even_in_a_compressed_row() {
        let total: f32 = TEST_COLUMNS.iter().map(|c| c.width).sum();
        let rect = row_rect();
        // Roomy, exactly-fitting, and two compressed rows (the last one
        // narrower than a single column's width).
        for right in [rect.right(), rect.left() + total + 4.0, total * 0.5, 40.0] {
            let anchors = column_anchors(rect.left(), right, &TEST_COLUMNS, 4.0);
            for (i, column) in TEST_COLUMNS.iter().enumerate() {
                let clip = column_clip_rect(rect, anchors[i], column.width);
                assert!(
                    clip.width() >= column.width,
                    "row right {right}, column {i}: clip width {} is narrower than \
                     its own {}pt budget, so in-range text loses leading glyphs",
                    clip.width(),
                    column.width
                );
                // The anchor is where the text is painted from; a slot that
                // did not contain it would clip everything.
                assert_eq!(clip.right(), anchors[i]);
            }
        }
    }

    /// The clip must still *bound* an overflowing value — the whole point of
    /// clipping at all. An out-of-range `ability_score`/`season_strength`
    /// gets cut off after one column's worth of glyphs instead of painting
    /// arbitrarily far left across the row, and where the row is wide enough
    /// for full-width columns that bound is exactly the previous column's
    /// anchor, so no neighbor is overpainted.
    #[test]
    fn column_clip_rect_bounds_overflow_to_one_column_width() {
        let rect = row_rect();
        let anchors = column_anchors(rect.left(), rect.right(), &TEST_COLUMNS, 4.0);
        for (i, column) in TEST_COLUMNS.iter().enumerate() {
            let clip = column_clip_rect(rect, anchors[i], column.width);
            assert_eq!(clip.width(), column.width);
            if i > 0 {
                assert_eq!(clip.left(), anchors[i - 1]);
            }
        }
    }

    /// The leftmost column has no earlier anchor to bound it, but is bounded
    /// all the same: its own width budget keeps an overflowing value off the
    /// player name to its left, well inside the row rect rather than running
    /// to (or past) the row's left edge.
    #[test]
    fn column_clip_rect_bounds_the_leftmost_column_short_of_the_row_edge() {
        let rect = row_rect();
        let anchors = column_anchors(rect.left(), rect.right(), &TEST_COLUMNS, 4.0);
        let clip = column_clip_rect(rect, anchors[0], TEST_COLUMNS[0].width);
        assert!(
            clip.left() > rect.left(),
            "leftmost clip {} should stop short of the row's left edge {}",
            clip.left(),
            rect.left()
        );
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
        // `ability_score`/`season_strength` are the two exceptions: they
        // render the full, un-abbreviated digit string (owner requirement),
        // so their widest plausible input is their real in-game ceiling —
        // ability score is a 5-digit stat (max 99_999) and season strength
        // is a 4-digit stat (max 9_999), per the repo owner — rather than
        // the field type's own ceiling (`u32::MAX`) or a `fmt_short`-derived
        // value. Do not "fix" these back to `u32::MAX`.
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
            // A death count is a 1-2 digit figure in practice; 99 is the
            // widest plausible one, not `u32::MAX`, same reasoning as the
            // in-game ceilings above.
            deaths: 99,
            ability_score: Some(99_999),
            season_strength: Some(9_999),
        };

        for (kind, column) in ColumnKind::ALL
            .into_iter()
            .zip(stat_columns_for(&ColumnKind::ALL))
        {
            let text = (column.text)(&widest_row);
            // Measured in the font that column is actually painted in
            // (issue #56's per-column emphasis), not one fixed font for all
            // of them — the DPS column is now both bold and larger than the
            // rest, so a single-font measurement would no longer be
            // testing what `draw_row` draws.
            let text_width = ctx.fonts_mut(|f| {
                f.layout_no_wrap(
                    text.clone(),
                    column_emphasis(kind).font(),
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
        let paints = share_bar_paints(rect, 1.0, None);
        assert_eq!(paints.fill_rect.width(), rect.width());
        assert_eq!(paints.accent_rect.width(), rect.width());
    }

    /// Issue #73: the accent line's width now matches the fill's width
    /// exactly, so a zero `bar_frac` collapses both to nothing rather than
    /// leaving a full-width accent line behind.
    #[test]
    fn share_bar_zero_frac_has_no_fill_and_no_accent_line() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 0.0, None);
        assert_eq!(paints.fill_rect.width(), 0.0);
        assert_eq!(paints.accent_rect.width(), 0.0);
    }

    /// Issue #73: the accent line used to always span the row's full width
    /// regardless of `bar_frac`; it now matches the fill's width exactly, so
    /// the underline stops exactly where the gradient fill stops.
    #[test]
    fn share_bar_accent_line_matches_the_fill_width() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 0.4, None);
        assert_eq!(paints.fill_rect.width(), rect.width() * 0.4);
        assert_eq!(paints.accent_rect.width(), paints.fill_rect.width());
    }

    /// The accent line is what makes the share boundary read crisply, so it
    /// must hug `rect`'s bottom edge rather than float somewhere inside the
    /// bar.
    #[test]
    fn share_bar_accent_line_sits_at_the_bottom_edge() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 0.5, None);
        assert_eq!(paints.accent_rect.bottom(), rect.bottom());
        assert_eq!(paints.accent_rect.height(), SHARE_BAR_ACCENT_THICKNESS);
    }

    /// A row short enough that the fixed accent thickness would exceed its
    /// height must clamp the accent line down to the row height instead of
    /// spilling past the row's top edge.
    #[test]
    fn share_bar_accent_thickness_clamps_at_a_tiny_row_height() {
        let tiny_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 1.0));
        let paints = share_bar_paints(tiny_rect, 0.5, None);
        assert!(paints.accent_rect.height() <= tiny_rect.height());
        assert_eq!(paints.accent_rect.top(), tiny_rect.top());
    }

    /// The fill must stay markedly more translucent than the accent line's
    /// right (fully opaque) end — that alpha gap is what lets the accent
    /// line carry the crisp share boundary while the fill reads as a subtle
    /// backdrop.
    #[test]
    fn share_bar_fill_is_fainter_than_its_accent_line() {
        let rect = share_bar_rect();
        let paints = share_bar_paints(rect, 0.5, None);
        assert!(paints.fill_bottom.a() < paints.accent_right.a());
    }

    #[test]
    fn share_bar_fill_grades_from_transparent_to_its_bottom_alpha() {
        let paints = share_bar_paints(share_bar_rect(), 0.5, None);
        let mesh = vertical_gradient_mesh(
            paints.fill_rect,
            egui::Color32::TRANSPARENT,
            paints.fill_bottom,
        );
        assert_eq!(mesh.vertices.len(), 4);
        // left_top, right_top, left_bottom, right_bottom (`gradient_mesh`'s
        // vertex order): the two top vertices are transparent, the two
        // bottom ones are `fill_bottom`.
        assert_eq!(mesh.vertices[0].color.a(), 0);
        assert_eq!(mesh.vertices[1].color.a(), 0);
        assert_eq!(mesh.vertices[2].color.a(), paints.fill_bottom.a());
        assert_eq!(mesh.vertices[3].color.a(), paints.fill_bottom.a());
    }

    #[test]
    fn share_bar_accent_grades_left_to_right() {
        let paints = share_bar_paints(share_bar_rect(), 0.5, None);
        let mesh =
            horizontal_gradient_mesh(paints.accent_rect, paints.accent_left, paints.accent_right);
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.vertices[0].color.a(), paints.accent_left.a());
        assert_eq!(mesh.vertices[2].color.a(), paints.accent_left.a());
        assert_eq!(mesh.vertices[1].color.a(), paints.accent_right.a());
        assert_eq!(mesh.vertices[3].color.a(), paints.accent_right.a());
    }

    // -- top-row damage scaling (issue #73) ----------------------------------

    /// Bars now scale to the *top row's* damage, not to each row's share of
    /// total encounter damage: the top row's bar always fills the full
    /// width, and a row with exactly half the top row's damage gets a
    /// half-width bar — even when neither figure is 100%/50% of the
    /// encounter's total damage (here the total across all rows would be
    /// far more than `2 * top_damage`).
    #[test]
    fn bar_width_scales_to_top_row_damage_not_share_of_total() {
        let rect = share_bar_rect();
        let top_damage = 10_000_i64;
        let half_damage = top_damage / 2;

        let top_paints = share_bar_paints(rect, row_bar_frac(top_damage, top_damage), None);
        let half_paints = share_bar_paints(rect, row_bar_frac(half_damage, top_damage), None);

        assert_eq!(top_paints.fill_rect.width(), rect.width());
        assert_eq!(half_paints.fill_rect.width(), rect.width() * 0.5);
    }

    /// An empty or degenerate snapshot (`top_damage` <= 0) must not divide
    /// by zero or produce a NaN/negative fraction.
    #[test]
    fn row_bar_frac_guards_against_a_non_positive_top_damage() {
        assert_eq!(row_bar_frac(500, 0), 0.0);
        assert_eq!(row_bar_frac(500, -10), 0.0);
    }

    /// The two quads must be contiguous (no gap, no overlap) and meet at the
    /// peak offset with the peak color on both sides of the seam.
    #[test]
    fn row_hover_quads_meet_at_the_peak_offset() {
        let rect = share_bar_rect();
        let quads = row_hover_quads(rect);
        let split_x = rect.left() + rect.width() * ROW_HOVER_PEAK_OFFSET;

        let (first_rect, first_left, first_right) = quads[0];
        assert_eq!(first_rect.left(), rect.left());
        assert_eq!(first_rect.right(), split_x);
        assert_eq!(first_left, egui::Color32::TRANSPARENT);

        let (second_rect, second_left, second_right) = quads[1];
        assert_eq!(second_rect.left(), split_x);
        assert_eq!(second_rect.right(), rect.right());
        assert_eq!(second_right, egui::Color32::TRANSPARENT);

        // Shared seam is the peak color on both sides.
        assert_eq!(first_right, second_left);
        assert_eq!(first_right.a(), ROW_HOVER_PEAK_ALPHA);
    }

    /// Runtime reads, not const-vs-const compares (clippy's
    /// `assertions_on_constants`) — same trick
    /// `font_scale_matches_the_source_metrics` uses.
    #[test]
    fn chrome_border_and_fill_are_translucent() {
        assert_eq!(PANEL_BORDER_COLOR.a(), 128);
        let fill_alpha = PANEL_FILL.a();
        assert!(fill_alpha > 0 && fill_alpha < 255, "{fill_alpha}");
    }

    /// The panel is deliberately *not* the source's slate `#232830` — that
    /// reads as washed-out grey over game footage. Lock the near-black.
    #[test]
    fn panel_fill_is_near_black_not_slate() {
        // Compare through the same constructor: `Color32` stores premultiplied
        // channels, so `to_tuple()` would not round-trip the (18, 18, 22).
        assert_eq!(
            PANEL_FILL,
            egui::Color32::from_rgba_unmultiplied(18, 18, 22, 200)
        );
    }

    // -- share bar role coloring (issue #44) --------------------------------
    //
    // Confirms the answer to issue #44's second open question directly:
    // the fill and accent line share the exact same role-derived RGB and
    // differ only in alpha (`SHARE_BAR_FILL_BOTTOM_ALPHA` vs the accent
    // line's left/right stops) — one hue, multiple alphas, not two
    // independently-colored paints.
    /// Compares against colors built the same way `share_bar_paints` builds
    /// them (`Color32::from_rgba_unmultiplied` on the same `(r, g, b)`, just
    /// a different alpha per paint) rather than trying to recover `(r, g,
    /// b)` back out of the painted `Color32`: `Color32` stores premultiplied
    /// components internally, so unmultiplying is lossy at a low alpha like
    /// the fill's (46 of 255) and would make this assertion flaky by up to a
    /// couple of units. Constructing both sides identically instead makes
    /// the comparison exact, and — since both expected colors are built
    /// from the one `expected_rgb` — directly proves "same RGB, alpha
    /// differs only by the fixed fill/accent split".
    fn assert_bar_hue(class: Option<Class>, expected_rgb: (u8, u8, u8)) {
        let paints = share_bar_paints(share_bar_rect(), 0.5, class);
        let (r, g, b) = expected_rgb;
        let expected_fill_bottom =
            egui::Color32::from_rgba_unmultiplied(r, g, b, SHARE_BAR_FILL_BOTTOM_ALPHA);
        let expected_accent_right = egui::Color32::from_rgba_unmultiplied(r, g, b, 255);
        assert_eq!(
            paints.fill_bottom, expected_fill_bottom,
            "fill color for {class:?}"
        );
        assert_eq!(
            paints.accent_right, expected_accent_right,
            "accent color for {class:?}"
        );
    }

    #[test]
    fn share_bar_uses_healer_hue_for_verdant_oracle() {
        assert_bar_hue(Some(Class::VerdantOracle), SHARE_BAR_RGB_HEALER);
    }

    #[test]
    fn share_bar_uses_healer_hue_for_beat_performer() {
        assert_bar_hue(Some(Class::BeatPerformer), SHARE_BAR_RGB_HEALER);
    }

    #[test]
    fn share_bar_uses_tank_hue_for_heavy_guardian() {
        assert_bar_hue(Some(Class::HeavyGuardian), SHARE_BAR_RGB_TANK);
    }

    #[test]
    fn share_bar_uses_tank_hue_for_shield_knight() {
        assert_bar_hue(Some(Class::ShieldKnight), SHARE_BAR_RGB_TANK);
    }

    #[test]
    fn share_bar_uses_damage_hue_for_stormblade() {
        assert_bar_hue(Some(Class::Stormblade), SHARE_BAR_RGB_DAMAGE);
    }

    #[test]
    fn share_bar_uses_fallback_hue_for_unknown_class() {
        assert_bar_hue(Some(Class::Unknown), SHARE_BAR_RGB_UNKNOWN);
    }

    #[test]
    fn share_bar_uses_fallback_hue_when_no_class_is_known() {
        assert_bar_hue(None, SHARE_BAR_RGB_UNKNOWN);
    }

    /// Guards against the fallback colliding with any role hue — such a
    /// collision would make an unclassified row silently look like a
    /// confirmed row of that role (this once happened with
    /// `SHARE_BAR_RGB_TANK`'s blue, before this test existed).
    #[test]
    fn share_bar_fallback_hue_differs_from_every_role_hue() {
        assert_ne!(SHARE_BAR_RGB_UNKNOWN, SHARE_BAR_RGB_HEALER);
        assert_ne!(SHARE_BAR_RGB_UNKNOWN, SHARE_BAR_RGB_TANK);
        assert_ne!(SHARE_BAR_RGB_UNKNOWN, SHARE_BAR_RGB_DAMAGE);
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
    fn default_settings_columns_get_one_anchor_each_pinned_to_the_right_margin() {
        let cols = Settings::default().ordered_columns();
        let anchors = column_anchors(0.0, 300.0, &stat_columns_for(&cols), 4.0);

        // One anchor per enabled column, whatever the default set currently
        // is (DPS / crit % / lucky % / deaths since issue #49) — the
        // invariant is the pairing and the right-edge pin, not the count.
        assert_eq!(anchors.len(), cols.len());
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

        // Both start disabled under the new default (`Dps`, `CritPct`,
        // `LuckyPct`), so toggling them is guaranteed to grow the set —
        // toggling `CritPct` itself would instead shrink it now that it's
        // on by default.
        settings.toggle(ColumnKind::AbilityScore);
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
        // The row-content budget alone (20 rows * 30pt) must fit inside the
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
        let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        // Decision 3: only the title->header and header->separator gaps
        // remain — `draw_rows` zeroes `item_spacing.y` for its own scope,
        // so there is no gap before the first row or between rows.
        let gaps = 2.0 * ITEM_SPACING_Y;
        let expected = TITLE_LINE_HEIGHT + BUTTON_ROW_HEIGHT + SEPARATOR_HEIGHT + rows + gaps;
        assert_eq!(default_inner_height(), expected);
        assert_eq!(default_inner_height(), 652.0);
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
            + COLUMN_RIGHT_MARGIN
            + HEADER_ROW_EXTRA_WIDTH;
        assert_eq!(default_inner_width(), expected);
        assert_eq!(default_inner_width(), 451.0);
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

    /// Reads back the accessible ("label") name AccessKit would announce for
    /// `id`, out of a full frame's `FullOutput::platform_output::
    /// accesskit_update`. `None`
    /// covers both "no accesskit update at all" and "a node exists but
    /// carries no label" — both mean a screen-reader user hears nothing.
    fn accessible_label(update: &egui::accesskit::TreeUpdate, id: egui::Id) -> Option<String> {
        update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())
            .and_then(|(_, node)| node.label())
            .map(str::to_string)
    }

    /// Same idea as `accessible_label`, but reads back where AccessKit says
    /// the labeled node painted. Every interactive `Response` fills in its
    /// AccessKit bounds from its own `rect` for free
    /// (`Response::fill_accesskit_node_common`), so this is what lets a test
    /// click a specific `draw_header_menu` item without that function
    /// handing back anything more than a `Ui` to paint into.
    fn accessible_rect_for_label(update: &egui::accesskit::TreeUpdate, label: &str) -> egui::Rect {
        let bounds = update
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.label() == Some(label))
                    .then(|| node.bounds())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"));
        egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        )
    }

    /// A single left-click (move, press, release, all in one frame) at
    /// `pos` — enough for `Response::clicked()` to fire on whatever gets
    /// allocated at `pos` during the very frame this `RawInput` drives,
    /// since `Context::run_ui` folds `new_input` into `InputState` before
    /// the paint closure runs.
    fn click_at(pos: egui::Pos2) -> egui::RawInput {
        let modifiers = egui::Modifiers::NONE;
        egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                },
            ],
            ..Default::default()
        }
    }

    /// Reset and Close are built from structurally near-identical blocks in
    /// `draw_header_menu` — icon, label, `.clicked()` guard, command
    /// dispatch — exactly the shape a copy-paste slip could swap without
    /// the compiler ever noticing. This drives real clicks through
    /// `Response::clicked()`, the same path `draw_header_menu` itself
    /// checks, rather than calling `tx_command.try_send` or
    /// `ctx.send_viewport_cmd` directly — a swapped pairing between the two
    /// blocks would still pass a test that only inspected the call sites in
    /// isolation.
    #[test]
    fn draw_header_menu_dispatches_reset_and_close_to_the_right_commands() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut collapse = CollapseState::default();

        // Frame 1: lay the menu out with no input, and read back where
        // AccessKit says "Reset" and "Close" actually painted — neither
        // button's rect is knowable ahead of a real `draw_header_menu` run.
        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_header_menu(
                ui,
                &ctx,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                &mut collapse,
                40.0,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let reset_pos = accessible_rect_for_label(&update, "Reset").center();
        let close_pos = accessible_rect_for_label(&update, "Close").center();
        layout.drop_without_applying_deltas();

        // Frame 2: click Reset.
        let output = ctx.run_ui(click_at(reset_pos), |ui| {
            draw_header_menu(
                ui,
                &ctx,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                &mut collapse,
                40.0,
            );
        });
        let reset_commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert_eq!(
            rx_command.try_recv().expect("Reset must send a command"),
            UiCommand::Reset
        );
        assert!(
            rx_command.try_recv().is_err(),
            "Reset must not also queue a second command"
        );
        assert!(
            !reset_commands.contains(&egui::ViewportCommand::Close),
            "Reset must not also close the window: {reset_commands:?}"
        );

        // Frame 3: click Close.
        let output = ctx.run_ui(click_at(close_pos), |ui| {
            draw_header_menu(
                ui,
                &ctx,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                &mut collapse,
                40.0,
            );
        });
        let close_commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert_eq!(
            rx_command.try_recv().expect("Close must send a command"),
            UiCommand::Quit
        );
        assert!(
            rx_command.try_recv().is_err(),
            "Close must not also queue a second command"
        );
        assert!(
            close_commands.contains(&egui::ViewportCommand::Close),
            "Close must also ask the viewport to close: {close_commands:?}"
        );
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

    // --- Manual (app-driven) window move/resize gestures, issue #11 ---

    /// A stand-in window rect, deliberately larger than `MIN_INNER_SIZE` on
    /// both axes and off the origin so a drift in either edge shows up.
    fn window_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 300.0))
    }

    #[test]
    fn a_move_offsets_the_origin_by_the_whole_pointer_delta() {
        assert_eq!(
            moved_window_origin(window_rect(), egui::vec2(20.0, -10.0)),
            egui::pos2(120.0, 40.0)
        );
    }

    #[test]
    fn dragging_the_east_edge_moves_only_the_right_edge() {
        let start = window_rect();
        let resized = resized_window_rect(
            start,
            egui::ResizeDirection::East,
            egui::vec2(30.0, 77.0),
            MIN_INNER_SIZE,
        );
        assert_eq!(resized.min, start.min);
        assert_eq!(resized.size(), egui::vec2(430.0, 300.0));
    }

    #[test]
    fn dragging_the_west_edge_moves_the_origin_and_anchors_the_right_edge() {
        let start = window_rect();
        let resized = resized_window_rect(
            start,
            egui::ResizeDirection::West,
            egui::vec2(-30.0, 40.0),
            MIN_INNER_SIZE,
        );
        assert_eq!(resized.min, egui::pos2(70.0, 50.0));
        assert_eq!(resized.max, start.max);
    }

    #[test]
    fn dragging_a_corner_resizes_on_both_axes_at_once() {
        let resized = resized_window_rect(
            window_rect(),
            egui::ResizeDirection::NorthEast,
            egui::vec2(25.0, -15.0),
            MIN_INNER_SIZE,
        );
        assert_eq!(
            resized,
            egui::Rect::from_min_max(egui::pos2(100.0, 35.0), egui::pos2(525.0, 350.0))
        );
    }

    #[test]
    fn a_trailing_edge_drag_never_shrinks_past_the_minimum_inner_size() {
        let start = window_rect();
        let resized = resized_window_rect(
            start,
            egui::ResizeDirection::SouthEast,
            egui::vec2(-1000.0, -1000.0),
            MIN_INNER_SIZE,
        );
        assert_eq!(resized.size(), MIN_INNER_SIZE);
        // The dragged edges are the trailing ones, so the origin is the
        // anchor and must not have moved.
        assert_eq!(resized.min, start.min);
    }

    #[test]
    fn clamping_a_leading_edge_drag_leaves_the_anchored_edges_put() {
        let start = window_rect();
        let resized = resized_window_rect(
            start,
            egui::ResizeDirection::NorthWest,
            egui::vec2(1000.0, 1000.0),
            MIN_INNER_SIZE,
        );
        assert_eq!(resized.size(), MIN_INNER_SIZE);
        // Dragging the left/top edges past the minimum must pin them at the
        // minimum, not push the right/bottom edges along with them.
        assert_eq!(resized.max, start.max);
        assert_eq!(resized.min, start.max - MIN_INNER_SIZE);
    }

    #[test]
    fn a_gesture_stays_active_until_it_is_ended() {
        let mut gesture = WindowGesture::default();
        assert_eq!(gesture.kind(), None);
        gesture.begin(GestureKind::Move, egui::pos2(10.0, 10.0), window_rect());
        assert_eq!(gesture.kind(), Some(GestureKind::Move));
        gesture.end();
        assert_eq!(gesture.kind(), None);
    }

    #[test]
    fn beginning_a_gesture_supersedes_one_still_running() {
        let mut gesture = WindowGesture::default();
        gesture.begin(GestureKind::Move, egui::pos2(10.0, 10.0), window_rect());
        let resize = GestureKind::Resize(egui::ResizeDirection::West);
        gesture.begin(resize, egui::pos2(0.0, 0.0), window_rect());
        assert_eq!(gesture.kind(), Some(resize));
        gesture.end();
        assert_eq!(gesture.kind(), None);
    }

    #[test]
    fn ending_an_idle_gesture_is_a_no_op() {
        // `end` runs on several exit paths (pointer released, focus lost,
        // drag cancelled), so it has to be safely repeatable — a second
        // release must not drop the exemption guard twice.
        let mut gesture = WindowGesture::default();
        gesture.end();
        gesture.begin(GestureKind::Move, egui::pos2(10.0, 10.0), window_rect());
        gesture.end();
        gesture.end();
        assert_eq!(gesture.kind(), None);
    }

    #[test]
    fn a_finished_resize_needs_a_frame_recompute() {
        // Issue #74: a resize is the gesture kind that can leave DWM's frame
        // stale, so its end must trigger `platform::force_frame_recompute`.
        let resize = GestureKind::Resize(egui::ResizeDirection::West);
        assert!(gesture_end_needs_frame_recompute(resize));
    }

    #[test]
    fn a_finished_move_does_not_need_a_frame_recompute() {
        // A pure move never changes the window's size, so there is nothing
        // for a DWM frame recompute to fix.
        assert!(!gesture_end_needs_frame_recompute(GestureKind::Move));
    }

    // --- death-count column (issue #49) ---------------------------------

    /// The dispatch that makes this column chrome rather than text. Exactly
    /// one column is a pill — if a second ever becomes one, `draw_row`'s
    /// single `paint_counter_pill` call needs revisiting first.
    #[test]
    fn deaths_is_the_only_column_painted_as_a_pill() {
        assert_eq!(column_emphasis(ColumnKind::Deaths), ColumnEmphasis::Counter);
        let pills: Vec<_> = ColumnKind::ALL
            .into_iter()
            .filter(|kind| column_emphasis(*kind).is_pill())
            .collect();
        assert_eq!(pills, vec![ColumnKind::Deaths]);
    }

    /// The counter shares the row's flat metric size (issue #62) — it is the
    /// only emphasis level painted as a pill rather than as bare text.
    #[test]
    fn the_counter_shares_the_row_metric_and_is_the_only_pill() {
        assert_eq!(ColumnEmphasis::Counter.font().size, FONT_SIZE_ROW);
        assert_eq!(FONT_SIZE_COUNTER, FONT_SIZE_ROW);
        assert!(ColumnEmphasis::Counter.is_pill());
        for other in [
            ColumnEmphasis::Value,
            ColumnEmphasis::Stat,
            ColumnEmphasis::Percent,
        ] {
            assert!(!other.is_pill(), "{other:?} should not be a pill");
        }
    }

    /// `widest_formatted_text_fits_its_column_width_budget` measures *text*
    /// against a column's width, which is the wrong budget for the one
    /// column painted as a pill: the chrome around the digits (padding on
    /// both ends, the skull's box, and the gap between them) is most of what
    /// this column actually occupies. This is that column's real budget
    /// check — the widest plausible count, laid out with the real fonts and
    /// grown by `pill_size`, has to fit `ColumnKind::Deaths.spec().width`.
    #[test]
    fn deaths_column_width_fits_the_whole_counter_pill() {
        let ctx = egui::Context::default();
        // Load the real (non-empty) default fonts, so glyph metrics match
        // what `paint_counter_pill` lays the value out with.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        let column = ColumnKind::Deaths.spec();
        // A death count is a 1-2 digit figure in practice; "99" is the
        // widest plausible one, the same reasoning the in-game ceilings in
        // `ColumnKind::spec` use, not the field type's `u32::MAX`.
        let pill = StatPill::counter("99", None, column.color);
        let text_size = ctx.fonts_mut(|f| {
            f.layout_no_wrap(pill.value.to_owned(), bold(pill.size), pill.value_color)
                .rect
                .size()
        });
        let pill_width = pill_size(text_size, pill.icon_side, ROW_HEIGHT).x;

        assert!(
            pill_width <= column.width,
            "the counter pill is {pill_width}pt wide, wider than its {}pt column budget",
            column.width
        );
        // And the budget is a budget, not a wildly oversized reservation —
        // every point here is a point the player name doesn't get.
        assert!(
            column.width - pill_width < 16.0,
            "the {}pt budget wastes {}pt on a {pill_width}pt pill",
            column.width,
            column.width - pill_width
        );
    }

    /// The pill obeys `column_anchors` exactly like a text column does: its
    /// right edge lands on the anchor (where right-aligned text would end),
    /// and it is centered in the row.
    #[test]
    fn counter_pill_is_right_aligned_on_its_column_anchor() {
        let row = row_rect();
        let size = egui::vec2(40.0, 14.0);
        let pill = counter_pill_rect(row, 250.0, size);

        assert_eq!(pill.right(), 250.0);
        assert_eq!(pill.size(), size);
        assert_eq!(pill.center().y, row.center().y);
    }

    /// A digit-count change must move the pill's *left* edge only — the
    /// anchored right edge is what keeps the column steady between frames.
    #[test]
    fn a_wider_count_grows_the_pill_leftwards_from_its_anchor() {
        let row = row_rect();
        let narrow = counter_pill_rect(row, 250.0, egui::vec2(36.0, 14.0));
        let wide = counter_pill_rect(row, 250.0, egui::vec2(44.0, 14.0));

        assert_eq!(narrow.right(), wide.right());
        assert!(wide.left() < narrow.left());
    }

    /// The pill is clipped to its own column slot like every other column
    /// (`column_clip_rect`), and at full column width it fits that slot
    /// outright rather than relying on the clip to save it.
    #[test]
    fn counter_pill_sits_inside_its_own_column_slot() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        let column = ColumnKind::Deaths.spec();
        let row = row_rect();
        let anchor = row.right() - COLUMN_RIGHT_MARGIN;
        let pill = StatPill::counter("99", None, column.color);
        let text_size = ctx.fonts_mut(|f| {
            f.layout_no_wrap(pill.value.to_owned(), bold(pill.size), pill.value_color)
                .rect
                .size()
        });
        let pill_rect = counter_pill_rect(
            row,
            anchor,
            pill_size(text_size, pill.icon_side, row.height()),
        );
        let clip = column_clip_rect(row, anchor, column.width);

        assert!(
            clip.contains_rect(pill_rect),
            "pill {pill_rect:?} escapes its column slot {clip:?}"
        );
    }

    /// The pill can never be taller than the row it sits in, whatever the
    /// font metrics turn out to be — an overflowing pill would paint over
    /// the rows above and below it.
    #[test]
    fn counter_pill_never_outgrows_its_row() {
        for text_height in [8.0, 13.0, 40.0] {
            let size = pill_size(
                egui::vec2(12.0, text_height),
                COUNTER_GLYPH_SIDE,
                ROW_HEIGHT,
            );
            assert!(size.y <= ROW_HEIGHT, "a {text_height}pt text overflowed");
        }
    }

    /// The counter's own styling, as the reference render shows it: skull
    /// first, then the count; the row's flat metric size; and a skull
    /// dimmer than the digits beside it (issue #62: the digits are now
    /// plain white, `DEATH_COUNT_RGB`, so the pill's `#1fff` background is
    /// what separates the counter from the row, not a dimmer digit color).
    #[test]
    fn counter_pill_leads_with_a_dimmed_skull() {
        let color =
            egui::Color32::from_rgb(DEATH_COUNT_RGB.0, DEATH_COUNT_RGB.1, DEATH_COUNT_RGB.2);
        let pill = StatPill::counter("3", None, color);

        assert!(pill.icon_first, "the reference reads skull-then-count");
        assert_eq!(pill.size, FONT_SIZE_COUNTER);
        assert_eq!(pill.value_color, color);
        assert_eq!(pill.icon_color, COUNTER_ICON_COLOR);
        assert_eq!(pill.icon_side, COUNTER_GLYPH_SIDE);
        // The skull is dimmer than the (now white) digits beside it — by
        // alpha (`#5fff`), not by a darker RGB, now that the glyph is a
        // rasterized icon rather than a stroked shape.
        assert!(COUNTER_ICON_COLOR.a() < 255);
        // And not the header's accent blue — the row's skull is chrome, not
        // an accent (see `COUNTER_ICON_COLOR`).
        assert_ne!(pill.icon_color, PILL_ICON_COLOR);
    }

    /// Walks a painted `Shape`, collecting every `Shape::Mesh`'s texture id
    /// — the counterpart to `collect_text_shapes`, for the one pill glyph
    /// that is blitted rather than stroked.
    fn collect_image_textures(shape: &egui::Shape, out: &mut Vec<egui::TextureId>) {
        match shape {
            egui::Shape::Mesh(mesh) => out.push(mesh.texture_id),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_image_textures(s, out);
                }
            }
            _ => {}
        }
    }

    /// Renders one counter pill for `icon` and hands back every texture id
    /// it blitted. Text shapes upload through the font atlas, so the pill's
    /// digits show up as the font texture — the assertions below compare
    /// against the specific skull texture rather than counting blits.
    fn counter_pill_textures(icon: Option<egui::TextureId>) -> Vec<egui::TextureId> {
        let ctx = egui::Context::default();
        let mut textures = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint_counter_pill(
                ui.painter(),
                row_rect(),
                row_rect().right(),
                StatPill::counter("2", icon, egui::Color32::WHITE),
            );
        });
        for clipped in &output.shapes {
            collect_image_textures(&clipped.shape, &mut textures);
        }
        output.drop_without_applying_deltas();
        textures
    }

    /// The skull is the vendored `assets/icons/glyphs/skull.png` texture,
    /// blitted — not a hand-painted approximation. This is what would fail
    /// if `paint_stat_pill`'s icon blit ever stopped drawing the asset.
    #[test]
    fn the_counter_pill_blits_its_skull_texture() {
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "test-skull",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );
        let textures = counter_pill_textures(Some(texture.id()));
        assert!(
            textures.contains(&texture.id()),
            "the skull texture was never painted: {textures:?}"
        );
    }

    /// A skull whose PNG failed to decode degrades to an empty icon box —
    /// the count still paints, nothing panics, and no other texture is
    /// substituted for it (see `StatPill::icon`).
    #[test]
    fn a_missing_skull_texture_paints_an_empty_icon_box() {
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "test-skull-absent",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );
        let textures = counter_pill_textures(None);
        assert!(!textures.contains(&texture.id()));
    }

    // --- collapse to header, issue #54 -----------------------------------

    /// The collapsed height *is* the header band — no second constant to
    /// drift — and it is well under the normal minimum inner height, which
    /// is exactly why the min-inner-size floor has to move with the state.
    #[test]
    fn a_collapsed_overlay_is_shorter_than_the_normal_minimum_height() {
        let button_row = BUTTON_ROW_HEIGHT;
        for has_subtitle in [false, true] {
            let band = header_band_height(has_subtitle, button_row);
            assert!(
                band < MIN_INNER_SIZE.y,
                "a {band}pt band (subtitle: {has_subtitle}) would not need the floor lowered"
            );
            assert!(band < default_inner_height());
        }
    }

    /// The band — and therefore the collapsed height — is not a constant: a
    /// dungeon subtitle appearing mid-collapse makes it taller, which is the
    /// case `CollapseSync::Request` exists for.
    #[test]
    fn the_collapsed_height_depends_on_the_subtitle() {
        let button_row = egui::Style::default().spacing.interact_size.y;
        assert!(header_band_height(true, button_row) > header_band_height(false, button_row));
    }

    fn collapsed_at(requested: f32, settled: bool) -> Collapsed {
        Collapsed {
            restore_height: 488.0,
            requested_height: requested,
            settled,
        }
    }

    /// Viewport commands are queued and applied later, so the frames between
    /// asking for the collapsed height and getting it must not be read as
    /// somebody else resizing the window.
    #[test]
    fn collapse_sync_waits_for_an_in_flight_resize_instead_of_expanding() {
        assert_eq!(
            collapse_sync(collapsed_at(40.0, false), 40.0, 488.0),
            CollapseSync::Hold
        );
    }

    #[test]
    fn collapse_sync_settles_once_the_window_reaches_the_requested_height() {
        assert_eq!(
            collapse_sync(collapsed_at(40.0, false), 40.0, 40.0),
            CollapseSync::Settle
        );
        // And stays put afterwards.
        assert_eq!(
            collapse_sync(collapsed_at(40.0, true), 40.0, 40.0),
            CollapseSync::Hold
        );
    }

    /// Issue #53's tray "Reset Window" puts the overlay back at the expanded
    /// default size behind the app's back. A collapsed overlay must notice
    /// and expand, or it would sit full-height painting nothing but a header.
    #[test]
    fn collapse_sync_expands_when_something_else_resizes_the_window() {
        assert_eq!(
            collapse_sync(collapsed_at(40.0, true), 40.0, default_inner_height()),
            CollapseSync::Expand
        );
    }

    /// A vertical drag-resize reaches the same branch, so pulling the bottom
    /// edge of a collapsed overlay opens it.
    #[test]
    fn dragging_a_collapsed_overlay_taller_expands_it() {
        assert_eq!(
            collapse_sync(collapsed_at(40.0, true), 40.0, 120.0),
            CollapseSync::Expand
        );
    }

    /// Sub-point wobble under fractional DPI scaling is not a resize — the
    /// same tolerance `GESTURE_EPSILON` buys the drag gestures.
    #[test]
    fn collapse_sync_tolerates_sub_point_height_wobble() {
        assert_eq!(
            collapse_sync(collapsed_at(40.0, true), 40.0, 40.4),
            CollapseSync::Hold
        );
    }

    /// A subtitle appearing while collapsed grows the band; the collapse
    /// re-requests the new height rather than mistaking its own moved target
    /// for somebody else's resize.
    #[test]
    fn collapse_sync_re_requests_when_the_band_changes_under_it() {
        let button_row = egui::Style::default().spacing.interact_size.y;
        let without = header_band_height(false, button_row);
        let with = header_band_height(true, button_row);

        assert_eq!(
            collapse_sync(collapsed_at(without, true), with, without),
            CollapseSync::Request(with)
        );
    }

    /// The resize floor tracks the state: normal while expanded, lowered to
    /// the collapsed height while collapsed — and only ever on the vertical
    /// axis, since collapsing never touches the width.
    #[test]
    fn the_min_inner_size_floor_follows_the_collapse_state() {
        let mut state = CollapseState::default();
        assert_eq!(state.min_inner_size(), MIN_INNER_SIZE);

        state.collapsed = Some(collapsed_at(40.0, true));
        assert_eq!(state.min_inner_size(), egui::vec2(MIN_INNER_SIZE.x, 40.0));
        assert!(state.min_inner_size().y < MIN_INNER_SIZE.y);
    }

    /// With the floor lowered, a purely horizontal drag on a collapsed
    /// window keeps its collapsed height instead of being clamped back up to
    /// the expanded 90pt minimum — which would silently expand it.
    #[test]
    fn a_horizontal_drag_does_not_clamp_a_collapsed_window_taller() {
        let state = CollapseState {
            collapsed: Some(collapsed_at(40.0, true)),
        };
        let start = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 40.0));

        let resized = resized_window_rect(
            start,
            egui::ResizeDirection::East,
            egui::vec2(30.0, 0.0),
            state.min_inner_size(),
        );

        assert_eq!(resized.height(), 40.0);
        assert_eq!(resized.width(), 430.0);
    }

    /// Runs one frame with the window reporting an inner size of `size`,
    /// calling `body` from inside it exactly like `OverlayApp::ui` does, and
    /// returns every viewport command the frame queued.
    fn collapse_frame(
        state: &mut CollapseState,
        size: egui::Vec2,
        mut body: impl FnMut(&mut CollapseState, &egui::Context),
    ) -> Vec<egui::ViewportCommand> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| body(state, ui.ctx()));
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();
        commands
    }

    fn inner_sizes(commands: &[egui::ViewportCommand]) -> Vec<egui::Vec2> {
        commands
            .iter()
            .filter_map(|cmd| match cmd {
                egui::ViewportCommand::InnerSize(size) => Some(*size),
                _ => None,
            })
            .collect()
    }

    fn min_inner_sizes(commands: &[egui::ViewportCommand]) -> Vec<egui::Vec2> {
        commands
            .iter()
            .filter_map(|cmd| match cmd {
                egui::ViewportCommand::MinInnerSize(size) => Some(*size),
                _ => None,
            })
            .collect()
    }

    /// Collapsing asks for exactly the band height, at the unchanged width,
    /// and lowers the OS floor to match — without that second command winit
    /// would simply refuse the sub-minimum size.
    #[test]
    fn collapsing_asks_for_the_band_height_and_lowers_the_os_floor() {
        let mut state = CollapseState::default();
        let size = egui::vec2(432.0, 488.0);

        let commands = collapse_frame(&mut state, size, |state, ctx| state.toggle(ctx, 40.0));

        assert!(state.is_collapsed());
        assert_eq!(inner_sizes(&commands), vec![egui::vec2(432.0, 40.0)]);
        assert_eq!(
            min_inner_sizes(&commands),
            vec![egui::vec2(MIN_INNER_SIZE.x, 40.0)]
        );
    }

    /// The round trip: collapsing remembers the height the window had, and
    /// expanding restores exactly it — not the default, and not whatever the
    /// collapsed window happened to measure.
    #[test]
    fn collapse_then_expand_round_trips_to_the_same_height() {
        let mut state = CollapseState::default();
        let expanded = egui::vec2(432.0, 313.0);

        collapse_frame(&mut state, expanded, |state, ctx| state.toggle(ctx, 40.0));
        assert!(state.is_collapsed());

        // The window really is the band height by the time the chevron is
        // clicked again — the pre-collapse height has to come from the state,
        // not from the live window.
        let commands = collapse_frame(&mut state, egui::vec2(432.0, 40.0), |state, ctx| {
            state.toggle(ctx, 40.0)
        });

        assert!(!state.is_collapsed());
        assert_eq!(inner_sizes(&commands), vec![expanded]);
        assert_eq!(min_inner_sizes(&commands), vec![MIN_INNER_SIZE]);
    }

    /// The expand path restores the normal floor *before* asking for the
    /// bigger size, so no intermediate state asks for a size the floor in
    /// force would reject.
    #[test]
    fn expanding_raises_the_floor_before_it_asks_for_the_larger_size() {
        let mut state = CollapseState::default();
        collapse_frame(&mut state, egui::vec2(432.0, 488.0), |state, ctx| {
            state.toggle(ctx, 40.0)
        });

        let commands = collapse_frame(&mut state, egui::vec2(432.0, 40.0), |state, ctx| {
            state.toggle(ctx, 40.0)
        });

        let floor = commands
            .iter()
            .position(|cmd| matches!(cmd, egui::ViewportCommand::MinInnerSize(_)))
            .expect("expanding must restore the floor");
        let size = commands
            .iter()
            .position(|cmd| matches!(cmd, egui::ViewportCommand::InnerSize(_)))
            .expect("expanding must resize the window");
        assert!(floor < size);
    }

    /// Issue #53's "Reset Window" end to end: the tray command resizes the
    /// window to the expanded default behind the app's back, and the next
    /// `sync` expands — keeping the height the reset chose, and restoring the
    /// normal floor rather than fighting it back down to the band.
    #[test]
    fn a_reset_window_while_collapsed_expands_and_keeps_the_reset_height() {
        let mut state = CollapseState {
            collapsed: Some(collapsed_at(40.0, true)),
        };

        let reset = egui::vec2(432.0, default_inner_height());
        let commands = collapse_frame(&mut state, reset, |state, ctx| state.sync(ctx, 40.0));

        assert!(!state.is_collapsed());
        assert_eq!(min_inner_sizes(&commands), vec![MIN_INNER_SIZE]);
        assert!(
            inner_sizes(&commands).is_empty(),
            "expanding on somebody else's resize must not overwrite their height"
        );
    }

    /// `sync` is a no-op while expanded — it must not queue a resize on
    /// every one of the overlay's ~10 frames a second. (Only the size
    /// commands are inspected: egui queues chrome commands of its own for
    /// every frame, and none of them are this module's business.)
    #[test]
    fn syncing_an_expanded_overlay_never_resizes_the_window() {
        let mut state = CollapseState::default();
        let commands = collapse_frame(&mut state, egui::vec2(432.0, 488.0), |state, ctx| {
            state.sync(ctx, 40.0)
        });
        assert!(!state.is_collapsed());
        assert!(inner_sizes(&commands).is_empty());
        assert!(min_inner_sizes(&commands).is_empty());
    }

    /// A settled collapse holds still too, for the same reason — it must not
    /// re-request the height it is already at.
    #[test]
    fn a_settled_collapse_never_re_requests_its_height() {
        let mut state = CollapseState {
            collapsed: Some(collapsed_at(40.0, true)),
        };
        let commands = collapse_frame(&mut state, egui::vec2(432.0, 40.0), |state, ctx| {
            state.sync(ctx, 40.0)
        });
        assert!(state.is_collapsed());
        assert!(inner_sizes(&commands).is_empty());
        assert!(min_inner_sizes(&commands).is_empty());
    }

    // --- the chevron itself (issue #54) ----------------------------------

    fn title_row() -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(default_inner_width(), TITLE_LINE_HEIGHT),
        )
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
