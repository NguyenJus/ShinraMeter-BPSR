//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in.

use std::time::Duration;

use bpsr_meter::{PlayerRow, Snapshot};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::settings::{self, ColumnKind, Settings};

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
}

impl OverlayApp {
    pub fn new(
        rx_snapshot: Receiver<Snapshot>,
        tx_command: Sender<UiCommand>,
        tx_settings: Sender<Settings>,
    ) -> Self {
        Self {
            snapshot: Snapshot {
                duration_ms: 0,
                total_damage: 0,
                total_dps: 0.0,
                rows: Vec::new(),
            },
            status: StatusLine::Ok,
            settings: settings::load(),
            rx_snapshot,
            tx_command,
            tx_settings,
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
                );

                if let StatusLine::Error(msg) = &self.status {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), msg.as_str());
                }

                ui.separator();
                draw_rows(ui, &self.snapshot, &self.settings.ordered_columns());
            });

        // ~10 Hz.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
) {
    // The whole header band is the drag surface, registered *before* the row's
    // contents so the buttons drawn into it end up on top and still get their
    // clicks. Grabbing a single glyph was too small a target to hit.
    let band = {
        let mut rect = ui.available_rect_before_wrap();
        rect.max.y = rect.min.y + ui.spacing().interact_size.y;
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

    ui.horizontal(|ui| {
        // Purely an affordance — the band above is what actually drags.
        ui.label("☰");

        ui.label(fmt_duration(snapshot.duration_ms));
        ui.label(format!("{} DPS", fmt_short(snapshot.total_dps as i64)));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // U+2715 (✕) isn't covered by any vendored `epaint_default_fonts`
            // TTF and rendered as a tofu square (issue #14). U+00D7 (×) is
            // covered by the default proportional font (Ubuntu-Light /
            // Hack-Regular), which is what this button actually uses.
            if ui.button("×").clicked() {
                let _ = tx_command.try_send(UiCommand::Quit);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Plain ASCII, covered by every vendored font (issue #14).
            //
            // There's no tray icon and no other in-app restore path: this
            // relies entirely on the OS taskbar entry to un-minimize.
            // `viewport()` below never calls `.with_taskbar(false)` (which
            // is what would hide it via `skip_taskbar`), so the window
            // keeps the default winit/OS taskbar presence even though it's
            // borderless and always-on-top. If `viewport()` ever gains a
            // taskbar-hiding or tool-window setting, this button needs a
            // real restore mechanism first.
            if ui.button("_").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if ui.button("Reset").clicked() {
                let _ = tx_command.try_send(UiCommand::Reset);
            }
            // Plain ASCII, covered by every vendored font (issue #14) —
            // the gear glyph (U+2699) isn't in either bundled TTF.
            draw_settings_menu(ui, settings, tx_settings);
        });
    });
}

/// The settings menu: a compact dropdown (egui's `menu_button`, so it needs
/// no extra open/closed state of its own) letting the user toggle which
/// stat columns render (issue #13).
fn draw_settings_menu(ui: &mut egui::Ui, settings: &mut Settings, tx_settings: &Sender<Settings>) {
    ui.menu_button("S", |ui| {
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
    });
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

fn draw_rows(ui: &mut egui::Ui, snapshot: &Snapshot, columns: &[ColumnKind]) {
    // The enabled-column set (and therefore the column widths and their
    // anchors) is identical for every row in a frame, so both are computed
    // once here rather than once per row inside `draw_row`.
    let stat_columns = stat_columns_for(columns);
    let avail = ui.available_rect_before_wrap();
    let anchors = column_anchors(avail.left(), avail.right(), &stat_columns, 4.0);

    for row in &snapshot.rows {
        draw_row(ui, row, &stat_columns, &anchors);
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

fn draw_row(ui: &mut egui::Ui, row: &PlayerRow, columns: &[StatColumn], anchors: &[f32]) {
    let desired_size = egui::vec2(ui.available_width(), 20.0);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    // Proportional background bar scaled by this player's damage share.
    let bar_frac = (row.share_pct / 100.0).clamp(0.0, 1.0);
    let bar_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * bar_frac, rect.height()));
    ui.painter().rect_filled(
        bar_rect,
        2.0,
        egui::Color32::from_rgba_unmultiplied(60, 120, 220, 120),
    );

    let name = row_name(row);
    ui.painter().text(
        rect.left_center() + egui::vec2(4.0, 0.0),
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

/// Overlay window shape: always-on-top, borderless, transparent, small.
pub fn viewport() -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_always_on_top()
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_inner_size([340.0, 220.0])
        .with_min_inner_size([220.0, 90.0])
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
    /// "/s" suffix on top of `fmt_short`'s ~6-char max — this test fails
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
        // `fmt_short`'s 6-char maximum and `fmt_share`'s.
        assert_eq!(fmt_short(999_949), "999.9K");
        assert_eq!(fmt_share(100.0), "100.0%");
        let widest_row = PlayerRow {
            uid: 1,
            name: String::new(),
            class: None,
            damage: 999_949,
            dps: 999_949.0,
            share_pct: 100.0,
            crit_pct: 100.0,
            lucky_pct: 100.0,
            hits: 999_949,
            ability_score: Some(999_949),
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
