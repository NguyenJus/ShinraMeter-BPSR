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

// -- issue #120: the redesigned dropdown ------------------------------
//
// The whole menu is one popup with *pages*, not a stack of popups and
// disclosures. `draw_header` opens it as a single
// `egui::Popup::menu(&chevron_response)` with
// `close_behavior(CloseOnClickOutside)`, and that is now a deliberate
// rule rather than the workaround it started life as (issue #93): this
// menu holds toggles and a slider, so no click *inside* it may ever
// dismiss it implicitly. Action rows (Restart capture, the two exports,
// Reset to defaults, Minimize, Close) call `ui.close()` themselves;
// state rows — the column checkboxes, the opacity slider, and the page
// navigation rows — never do.
//
// The two tall, variable-height sections that used to live inline on the
// root (Columns and Background images) are pages of this same popup now.
// Clicking `Columns` swaps the body for the Columns page, which carries a
// back row; nothing new is opened. That kills three problems at once
// against the `CollapsingState` disclosure it replaces: there is no
// second popup layer to mistime or mis-position (issue #93's hover trap),
// no `show_toggle_button`/response-union hack needed to keep a full-row
// click target while still showing an icon, and no mid-menu height jump
// that the `Area`'s remembered size then has to chase (issue #231).

/// Fixed width, in points, of the header dropdown's body. Applied with
/// `ui.set_width` on *every* page so a drill-down can never reflow the
/// popup sideways: the user's pointer stays over the same column of
/// pixels when the body swaps, which is the whole reason pages are usable
/// in place of a flyout.
pub(crate) const HEADER_MENU_WIDTH: f32 = 248.0;

/// Height of every clickable row in the dropdown. Uniform on purpose —
/// the rows are a list, and a list whose items are sized by their own
/// content reads as an accident.
pub(crate) const MENU_ROW_HEIGHT: f32 = 24.0;

/// Vertical space above a section label, separating a group of rows from
/// the one before it (`menu_section`).
pub(crate) const MENU_SECTION_GAP: f32 = 6.0;

/// Left/right padding inside a row, between the popup's content edge and
/// the row's own ink.
pub(crate) const MENU_ROW_INSET: f32 = 8.0;

/// Width reserved for a row's leading icon, whether or not this
/// particular row has one — that reservation is exactly what makes an
/// icon-less row's label line up with an icon-bearing one above it, and
/// it is why `menu_row_layout` takes no icon argument at all.
pub(crate) const MENU_ICON_SLOT: f32 = 20.0;

/// Gap between the icon slot's right edge and where a row's label starts.
pub(crate) const MENU_ROW_LABEL_GAP: f32 = 4.0;

/// Row-label text size — the theme's proportional body font (`regular`),
/// not the monospace numerals the table uses.
const MENU_LABEL_FONT_SIZE: f32 = 13.0;

/// Section labels, group hints and the locked-column footer: deliberately
/// smaller and weaker than a row label so they read as annotation.
const MENU_SMALL_FONT_SIZE: f32 = 11.0;

/// A row's trailing value ("4 of 9", "78%", a column's sample). Monospace
/// so a column of them stays aligned digit-for-digit down the menu.
const MENU_TRAILING_FONT_SIZE: f32 = 12.0;

/// Minimum height a `menu_section`/hint line occupies, so a one-line
/// annotation still gets a consistent band even when its glyphs are
/// shorter than that.
const MENU_SMALL_LINE_HEIGHT: f32 = 16.0;

/// Corner rounding of a row's hover fill.
const MENU_ROW_ROUNDING: u8 = 4;

/// Width reserved for the Columns page's small "Reset" button, taken out
/// of the back row's own width so the two share one line.
const MENU_COLUMNS_RESET_WIDTH: f32 = 48.0;

/// Which page of the header dropdown is showing. Lives in egui temp
/// memory under `menu_page_id` rather than on `OverlayApp`, for the same
/// reason the Columns disclosure's open/closed flag did: it is pure
/// view state with no meaning once the popup is shut, and `draw_header`
/// resets it to `Root` on every frame the popup is closed
/// (`reset_menu_page`), so reopening the menu always lands on the root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MenuPage {
    #[default]
    Root,
    Columns,
    Backgrounds,
}

/// A navigation request produced by one frame of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuNav {
    /// Drill down into `MenuPage`.
    Open(MenuPage),
    /// The back row. Always returns to the root — see `next_page`.
    Back,
}

/// Applies `nav` to the page currently showing.
///
/// `Back` is deliberately unconditional rather than a per-page parent
/// pointer: the hierarchy is exactly one level deep by design (§1 of the
/// redesign — no nested popups, no nested pages), so a back row needs no
/// wiring of its own and a third page can be added without touching this.
pub(crate) fn next_page(nav: MenuNav) -> MenuPage {
    match nav {
        MenuNav::Back => MenuPage::Root,
        MenuNav::Open(page) => page,
    }
}

/// egui temp-memory key for `MenuPage`. A fixed `Id` rather than
/// `Ui::make_persistent_id` because the value is read and written from
/// two different `Ui`s — the popup body writes it, and `draw_header`
/// (outside the popup entirely) clears it — which a `Ui`-derived,
/// salted id could not address from both sides. There is exactly one
/// header dropdown in the process, so a global key is unambiguous.
pub(crate) fn menu_page_id() -> egui::Id {
    egui::Id::new("header_menu_page")
}

/// Puts the dropdown back on its root page. Called from `draw_header` on
/// every frame the popup is *not* open, so a menu that was closed while
/// drilled into Columns does not reopen there: a dropdown that remembers
/// a sub-page is a dropdown whose first click lands somewhere the user
/// did not ask for.
pub(crate) fn reset_menu_page(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(menu_page_id(), MenuPage::Root));
}

/// The three horizontal bands one dropdown row is painted into.
pub(crate) struct MenuRowRects {
    /// The reserved leading-icon slot, `icon_slot` wide and vertically
    /// centered in the row.
    pub icon: egui::Rect,
    /// Where the label paints, from just past the icon slot up to the
    /// trailing element.
    pub label: egui::Rect,
    /// Right-aligned trailing band, `trailing_width` wide — an empty rect
    /// pinned to the right content edge when there is nothing trailing.
    pub trailing: egui::Rect,
}

/// Splits a row rect into icon / label / trailing bands.
///
/// Pure and separate from the `Ui` that paints them, the same way
/// `chevron_rect` and `title_toggle_pill_rect` are: alignment across rows
/// is the entire point of the icon slot, and a headless test is the only
/// way this box can check it. Degrades to non-inverted (possibly
/// zero-width) rects in a row too narrow to hold everything, rather than
/// producing an inverted rect egui would then paint inside out.
pub(crate) fn menu_row_layout(
    row: egui::Rect,
    inset: f32,
    icon_slot: f32,
    trailing_width: f32,
) -> MenuRowRects {
    let left = row.left() + inset;
    let right = (row.right() - inset).max(left);
    let icon = egui::Rect::from_center_size(
        egui::pos2(left + icon_slot / 2.0, row.center().y),
        egui::Vec2::splat(icon_slot),
    );
    let label_left = (icon.right() + MENU_ROW_LABEL_GAP).min(right);
    let trailing_left = (right - trailing_width).max(label_left);
    let trailing = egui::Rect::from_min_max(
        egui::pos2(trailing_left, row.top()),
        egui::pos2(right.max(trailing_left), row.bottom()),
    );
    let label = egui::Rect::from_min_max(
        egui::pos2(label_left, row.top()),
        egui::pos2(trailing.left(), row.bottom()),
    );
    MenuRowRects {
        icon,
        label,
        trailing,
    }
}

/// What sits at the right-hand end of a `menu_row`.
pub(crate) enum Trailing<'a> {
    /// Nothing — the label runs to the row's right inset.
    None,
    /// A right-aligned monospace value, e.g. `"78%"`.
    Value(&'a str),
    /// A drill-down row: its summary value followed by the `▸`
    /// affordance, so one glance says both what the page currently holds
    /// and that it *is* a page.
    Page(&'a str),
    /// An action that opens an OS dialog. Renders as a `…` appended to
    /// the label itself rather than as a trailing element, which is where
    /// the platform convention puts it.
    Ellipsis,
}

/// One row of the header dropdown.
pub(crate) struct MenuRow<'a> {
    /// Optional leading icon; the slot is reserved either way.
    pub icon: Option<&'a egui::TextureHandle>,
    pub label: &'a str,
    pub trailing: Trailing<'a>,
    /// A disabled row senses hover only (so a tooltip still works) and
    /// paints its label weak, with no hover fill — it must read as inert,
    /// not merely dim.
    pub enabled: bool,
}

/// Paints one dropdown row and returns its `Response`.
///
/// Hand-painted rather than built from `egui::Button`: the row needs a
/// fixed height independent of whether it carries an icon, a label
/// aligned to a shared icon slot, and a right-aligned trailing element —
/// none of which `Button`'s intrinsic layout offers. The explicit
/// `widget_info` is what puts the row in the AccessKit tree under its
/// painted name, which is both the accessibility story and how the
/// header tests locate a row to click.
pub(crate) fn menu_row(ui: &mut egui::Ui, row: MenuRow<'_>) -> egui::Response {
    let sense = if row.enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), MENU_ROW_HEIGHT), sense);

    // The accessible name is the *painted* name, "…" included, so a
    // screen reader and a test both hear exactly what the row shows.
    let label = match row.trailing {
        Trailing::Ellipsis => format!("{}…", row.label),
        _ => row.label.to_owned(),
    };

    if ui.is_rect_visible(rect) {
        let (hover_fill, label_color, weak_color) = {
            let visuals = ui.visuals();
            (
                visuals.widgets.hovered.weak_bg_fill,
                if row.enabled {
                    visuals.text_color()
                } else {
                    visuals.weak_text_color()
                },
                visuals.weak_text_color(),
            )
        };
        if row.enabled && response.hovered() {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(MENU_ROW_ROUNDING),
                hover_fill,
            );
        }

        let trailing = match row.trailing {
            Trailing::Value(value) => Some(value.to_owned()),
            Trailing::Page(value) => Some(format!("{value} ▸")),
            Trailing::None | Trailing::Ellipsis => None,
        };
        let trailing_font = egui::FontId::monospace(MENU_TRAILING_FONT_SIZE);
        let trailing_width = trailing
            .as_ref()
            .map(|text| {
                ui.painter()
                    .layout_no_wrap(text.clone(), trailing_font.clone(), weak_color)
                    .size()
                    .x
            })
            .unwrap_or(0.0);

        let rects = menu_row_layout(rect, MENU_ROW_INSET, MENU_ICON_SLOT, trailing_width);
        if let Some(handle) = row.icon {
            // The slot is `MENU_ICON_SLOT` wide but the artwork keeps its
            // own `TOOLBAR_ICON_SIZE` footprint, centered in it — the slot
            // is alignment padding, not a size override.
            toolbar_icon_image(handle).paint_at(
                ui,
                egui::Rect::from_center_size(
                    rects.icon.center(),
                    egui::Vec2::splat(TOOLBAR_ICON_SIZE),
                ),
            );
        }
        // Laid out as a single-row, elided `LayoutJob` rather than
        // `ui.painter().text` (which never wraps, clips, or truncates)
        // so a label too long for the slot truncates with an ellipsis
        // instead of overrunning the trailing value.
        let mut label_job = egui::text::LayoutJob::single_section(
            label.clone(),
            egui::TextFormat {
                font_id: regular(MENU_LABEL_FONT_SIZE),
                color: label_color,
                ..Default::default()
            },
        );
        label_job.wrap = egui::text::TextWrapping {
            max_width: rects.label.width(),
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let label_galley = ui.painter().layout_job(label_job);
        let label_pos = egui::Align2::LEFT_CENTER
            .anchor_size(rects.label.left_center(), label_galley.size())
            .min;
        ui.painter().galley(label_pos, label_galley, label_color);
        if let Some(text) = trailing {
            ui.painter().text(
                rects.trailing.right_center(),
                egui::Align2::RIGHT_CENTER,
                text,
                trailing_font,
                weak_color,
            );
        }
    }

    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, row.enabled, &label));
    response
}

/// The `◂ …` row at the top of a drill-down page. Same geometry as
/// `menu_row` — it is a row of the same list — with the back affordance
/// painted into the icon slot the other rows reserve, so the label lines
/// up with theirs.
///
/// Takes an explicit `width` rather than claiming `available_width`
/// because the Columns page shares this line with its own "Reset" button.
pub(crate) fn menu_back_row(ui: &mut egui::Ui, label: &str, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width.max(0.0), MENU_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        let (hover_fill, color) = {
            let visuals = ui.visuals();
            (visuals.widgets.hovered.weak_bg_fill, visuals.text_color())
        };
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(MENU_ROW_ROUNDING),
                hover_fill,
            );
        }
        let rects = menu_row_layout(rect, MENU_ROW_INSET, MENU_ICON_SLOT, 0.0);
        ui.painter().text(
            rects.icon.center(),
            egui::Align2::CENTER_CENTER,
            "◂",
            regular(MENU_LABEL_FONT_SIZE),
            color,
        );
        ui.painter().text(
            rects.label.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            regular(MENU_LABEL_FONT_SIZE),
            color,
        );
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response
}

/// A weak, small, wrapped annotation line: section headers, a column
/// group's one-line hint, and the locked-column footer all use it.
/// Allocates its own height so the caller never has to guess how many
/// lines the text wrapped to.
fn menu_small_line(ui: &mut egui::Ui, text: &str) {
    let color = ui.visuals().weak_text_color();
    let width = ui.available_width();
    let wrap_width = (width - 2.0 * MENU_ROW_INSET).max(1.0);
    let galley = ui.painter().layout(
        text.to_owned(),
        regular(MENU_SMALL_FONT_SIZE),
        color,
        wrap_width,
    );
    let height = galley.size().y.max(MENU_SMALL_LINE_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    ui.painter().galley(
        egui::pos2(rect.left() + MENU_ROW_INSET, rect.top()),
        galley,
        color,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
}

/// A section header inside the dropdown, with `MENU_SECTION_GAP` of air
/// above it.
pub(crate) fn menu_section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(MENU_SECTION_GAP);
    menu_small_line(ui, text);
}

/// A hint or footer line — `menu_section` without the leading gap, so it
/// reads as attached to whatever it annotates.
pub(crate) fn menu_hint(ui: &mut egui::Ui, text: &str) {
    menu_small_line(ui, text);
}

/// One labelled group of column checkboxes on the Columns page.
pub(crate) struct ColumnGroup {
    pub title: &'static str,
    /// One line saying what the group's columns have in common — the
    /// labels alone don't distinguish "shown beside the name" from "its
    /// own stat column", which is exactly the distinction
    /// `renders_inline_with_name` (and therefore the `LastStatColumn`
    /// guard) turns on.
    pub hint: &'static str,
    pub columns: &'static [ColumnKind],
}

/// The Columns page's grouping of `ColumnKind::ALL`.
///
/// A partition, not a filter: every column appears in exactly one group,
/// and each group lists its members in `ColumnKind::ALL`'s canonical
/// left-to-right order, so the page reads in the same order the table
/// paints. `column_groups_cover_every_column_exactly_once_in_canonical_order`
/// holds both properties, which is what makes adding a `ColumnKind`
/// without listing it here a test failure rather than a silently
/// unreachable checkbox.
pub(crate) fn column_groups() -> &'static [ColumnGroup] {
    const GROUPS: &[ColumnGroup] = &[
        ColumnGroup {
            title: "Player",
            hint: "Shown beside the name, not as a stat column",
            columns: &[ColumnKind::AbilityScore, ColumnKind::SeasonStrength],
        },
        ColumnGroup {
            title: "Damage",
            hint: "Per-player totals for the current fight",
            columns: &[
                ColumnKind::Damage,
                ColumnKind::Dps,
                ColumnKind::SharePct,
                ColumnKind::Hits,
            ],
        },
        ColumnGroup {
            title: "Quality",
            hint: "Rates and survivability",
            columns: &[
                ColumnKind::CritPct,
                ColumnKind::LuckyPct,
                ColumnKind::Deaths,
            ],
        },
    ];
    GROUPS
}

/// Why a column's checkbox is held on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnLock {
    /// Issue #13's guard: this is the only visible column left.
    LastVisible,
    /// Issue #168's guard: this is the only column left that occupies a
    /// stat slot, so turning it off would leave the row a name with a
    /// blank grid beside it.
    LastStatColumn,
}

/// Whether `Settings::toggle` would refuse to turn `col` off, and why.
///
/// The two rules are `Settings::toggle`'s own, restated as a predicate so
/// the menu can *show* them — before this, a click on the last stat
/// column simply did nothing, with no greyed state and no explanation.
/// This is deliberately a second statement of the same logic rather than
/// a `Settings` method: `toggle` stays the enforcement point (a caller
/// that skips the menu still cannot break the invariant), and this stays
/// the presentation point.
pub(crate) fn column_toggle_lock(settings: &Settings, col: ColumnKind) -> Option<ColumnLock> {
    // Turning a column *on* is never guarded — both rules are about
    // removals.
    if !settings.is_visible(col) {
        return None;
    }
    if settings.visible_columns.len() <= 1 {
        return Some(ColumnLock::LastVisible);
    }
    if !col.renders_inline_with_name() && !settings.stat_columns().iter().any(|c| *c != col) {
        return Some(ColumnLock::LastStatColumn);
    }
    None
}

/// The user-facing reason a locked checkbox will not move.
pub(crate) fn column_lock_hint(lock: ColumnLock) -> &'static str {
    match lock {
        ColumnLock::LastVisible => "At least one column must stay visible",
        ColumnLock::LastStatColumn => "At least one stat column must stay visible",
    }
}

/// The root page's trailing summary for the Columns row, e.g. `"4 of 9"`.
pub(crate) fn visible_count_text(settings: &Settings) -> String {
    format!(
        "{} of {}",
        settings.visible_columns.len(),
        ColumnKind::ALL.len()
    )
}

/// The root page's trailing summary for the Opacity row. Rounded, not
/// truncated — a slider parked a hair under 0.8 reading "79%" would look
/// like an off-by-one to the user dragging it.
pub(crate) fn opacity_percent_text(opacity: f32) -> String {
    format!("{}%", (opacity * 100.0).round() as i32)
}

/// The root page's trailing summary for the Background images row.
pub(crate) fn backgrounds_summary_text(settings: &Settings) -> String {
    let configured = ImageSlot::ALL
        .iter()
        .filter(|slot| settings.background_image(**slot).is_some())
        .count();
    if configured == 0 {
        "none".to_owned()
    } else {
        format!("{configured} set")
    }
}

/// The Columns page's own Reset: restores the default column set and
/// *only* that.
///
/// Deliberately not `Settings::reset_to_defaults` (the root page's "Reset
/// to defaults" item): a reset button sitting on the Columns page must
/// mean "reset the columns", not silently discard the user's opacity,
/// window toggles and history retention as a side effect of looking at
/// this page.
pub(crate) fn reset_columns_to_default(settings: &mut Settings) {
    settings.visible_columns = Settings::default().visible_columns;
}

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
        // The region's name is the `menu_section` header above this row on
        // the Backgrounds page (issue #120), not a label inside it — so
        // the buttons start on the same inset as every other control.
        ui.add_space(MENU_ROW_INSET);
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

/// The header dropdown (issues #54, #71, #120): opened from
/// `menu_chevron` via `egui::Popup::menu`, one popup with drill-down
/// pages rather than a flat list with an inline disclosure in the middle
/// of it. See this module's `-- issue #120 --` block above for the close
/// -behavior rule and why Columns/Backgrounds are pages.
///
/// Issue #231: the body is wrapped in a vertical `ScrollArea` capped at
/// `header_menu_scroll_max_height`, so a page taller than the display
/// scrolls instead of pushing its own bottom rows off-screen where
/// nothing could reach them (the popup is not a native window; there is
/// no OS chrome to scroll it). The cap uses the full screen height rather
/// than the popup's on-screen headroom because egui does not expose the
/// latter until after the popup has been laid out once.
///
/// `ui.set_max_height` above the `ScrollArea` is issue #231's actual fix
/// and still matters with pages: `egui::Popup`'s `Area` remembers its
/// content size across frames and hands that same remembered height back
/// as this `Ui`'s `max_rect` next frame. A `ScrollArea::max_height` alone
/// cannot override that — its footprint is capped by whatever height the
/// `Ui` was *given* — so a frame committed at the root page's height
/// would keep reporting "I fit" and the `Area` would never learn the
/// taller Columns page needs more room. `set_max_height` overrides that
/// stale ceiling with the freshly computed cap every frame.
///
/// `ui.set_width(HEADER_MENU_WIDTH)` is applied for every page, so the
/// popup never changes width under the pointer when the body swaps.
// Issue #39: same reasoning as `draw_header`'s identical allow — one more
// history-view parameter tips this over clippy's default limit.
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
    ui.set_width(HEADER_MENU_WIDTH);
    ui.set_max_height(scroll_max_height);

    let page_id = menu_page_id();
    let page: MenuPage = ctx.data(|data| data.get_temp(page_id)).unwrap_or_default();
    let mut nav: Option<MenuNav> = None;

    egui::ScrollArea::vertical()
        .max_height(scroll_max_height)
        // Shrinks to the content's actual height whenever that's under the
        // cap, so a short page keeps painting with no reserved scrollbar
        // gutter and no extra bottom padding.
        .auto_shrink([false, true])
        .show(ui, |ui| {
            nav = match page {
                MenuPage::Root => draw_menu_root(
                    ui,
                    ctx,
                    tx_command,
                    settings,
                    tx_settings,
                    icons,
                    update_check,
                    tx_log_export,
                    quit_requested,
                ),
                MenuPage::Columns => draw_columns_page(ui, settings, tx_settings),
                MenuPage::Backgrounds => draw_backgrounds_page(ui, settings, tx_settings, icons),
            };
        });

    if let Some(nav) = nav {
        ctx.data_mut(|data| data.insert_temp(page_id, next_page(nav)));
    }
}

/// The dropdown's root page: three labelled sections (DISPLAY, SESSION,
/// APP) separated by a single `ui.separator()` each, never one after the
/// last. Returns the navigation request a drill-down row produced, if any.
///
/// Every row here is a `menu_row`, so all ten line up on one icon slot
/// whether or not they carry artwork. Only the action rows call
/// `ui.close()`; the Columns/Backgrounds rows and the opacity slider are
/// state, and dismissing the menu under them would be the bug issue #93
/// fixed.
#[allow(clippy::too_many_arguments)]
fn draw_menu_root(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tx_command: &Sender<UiCommand>,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
    icons: &Icons,
    update_check: &mut UpdateCheckState,
    tx_log_export: &Sender<LogExportOutcome>,
    quit_requested: &mut bool,
) -> Option<MenuNav> {
    let mut nav = None;

    menu_section(ui, "DISPLAY");

    // Issue #13's stat-column toggles, unchanged in behavior — the list
    // itself is a page now (issue #120) instead of a `CollapsingState`
    // disclosure that doubled the menu's height in place.
    let visible_count = visible_count_text(settings);
    if menu_row(
        ui,
        MenuRow {
            icon: icons.toolbar.get(ToolbarIcon::Settings),
            label: "Columns",
            trailing: Trailing::Page(&visible_count),
            enabled: true,
        },
    )
    .clicked()
    {
        nav = Some(MenuNav::Open(MenuPage::Columns));
    }

    // Issue #166: a single overlay-wide value, so it stays on the root
    // rather than earning a page of its own — the label row carries the
    // current reading and the rail sits directly under it.
    let opacity_text = opacity_percent_text(settings.opacity);
    menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Opacity",
            trailing: Trailing::Value(&opacity_text),
            enabled: true,
        },
    );
    // Issue #182: the rail spans the full 0%-100%, floor included —
    // `Settings::OPACITY_MIN` documents why a fully transparent backdrop
    // stays recoverable. The one place an `Opacity` goes back to a bare
    // `f32`: `Slider` edits the raw fraction in place and
    // `Settings::set_opacity` re-clamps it on the way back out.
    let mut opacity = Opacity::new(settings.opacity).as_f32();
    let opacity_response = ui
        .horizontal(|ui| {
            ui.add_space(MENU_ROW_INSET);
            // Issue #235: `Slider` has no width builder of its own — its
            // rail is sized entirely off `Spacing::slider_width`, a fixed
            // ~100pt default. This is the only `Slider` in the overlay, so
            // widening the shared spacing value inside this `horizontal`
            // can't affect anything painted after it.
            ui.spacing_mut().slider_width = (ui.available_width() - MENU_ROW_INSET).max(1.0);
            ui.add(
                egui::Slider::new(&mut opacity, Settings::OPACITY_MIN..=Settings::OPACITY_MAX)
                    .show_value(false),
            )
        })
        .inner;
    if opacity_response.changed() {
        // Applied immediately (same frame): this mutates the caller's
        // `&mut Settings` in place, and `OverlayApp::ui` reads
        // `self.settings.opacity` fresh when it builds the panel `Frame`
        // right after `draw_header` returns.
        settings.set_opacity(opacity);
        // Persisting is blocking file IO (`fs::write` + `fs::rename`), so
        // it must not run on this render thread — hand the new value to
        // the dedicated settings-writer thread instead. A disconnected
        // receiver is not fatal: the in-memory `settings` the UI already
        // mutated stays correct for the rest of this session.
        let _ = tx_settings.send(settings.clone());
    }

    // Issues #121/#253: both custom regions behind one row, because the
    // two are the same control twice and read as a pair — and their
    // status lines are the tallest, most variable content in the menu,
    // which is exactly what a page is for.
    let backgrounds = backgrounds_summary_text(settings);
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Background images",
            trailing: Trailing::Page(&backgrounds),
            enabled: true,
        },
    )
    .clicked()
    {
        nav = Some(MenuNav::Open(MenuPage::Backgrounds));
    }

    ui.separator();
    menu_section(ui, "SESSION");

    // Issue #214: the only in-process recovery from a capture wedge that
    // no new TCP connection happens to clear. Deliberately unconfirmed and
    // never disabled: the request is a latching flag rather than a queue,
    // so clicking twice is one restart, and anyone reaching for it is
    // already looking at a meter that has stopped moving.
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Restart packet capture",
            trailing: Trailing::None,
            enabled: true,
        },
    )
    .clicked()
    {
        let _ = tx_command.try_send(UiCommand::RestartCapture);
        ui.close();
    }

    // Issue #220: a user hitting a bug has no in-app way to hand over the
    // logs `logging::init` already writes for exactly this purpose. Never
    // a fixed or hidden path — the save dialog is what lets the user pick
    // the destination themselves. The dialog is called inline on this
    // thread (`platform::choose_log_export_path`'s doc comment explains
    // why a modal the OS is already blocking on needs no thread); the copy
    // that follows goes to a spawned thread.
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Export logs",
            trailing: Trailing::Ellipsis,
            enabled: true,
        },
    )
    .clicked()
    {
        if let Some(dest) =
            crate::platform::choose_log_export_path(crate::logging::EXPORT_DEFAULT_FILENAME)
        {
            start_log_export(dest, tx_log_export.clone());
        }
        ui.close();
    }

    // The whole-session handover: logs plus the packet-inspection dump
    // ring, `settings.json`, and a `manifest.json` describing all of it.
    // `history.sqlite` is deliberately never included (plaintext party
    // member names) and `manifest.json` says so. Same dialog-inline /
    // copy-on-a-spawned-thread split as "Export logs", and the same reply
    // channel.
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Export session bundle",
            trailing: Trailing::Ellipsis,
            enabled: true,
        },
    )
    .clicked()
    {
        if let Some(dest) =
            crate::platform::choose_bundle_export_path(bundle::EXPORT_BUNDLE_DEFAULT_DIRNAME)
        {
            start_bundle_export(dest, tx_log_export.clone());
        }
        ui.close();
    }

    ui.separator();
    menu_section(ui, "APP");

    // Issue #171: manual-only, per the issue — there is no automatic or
    // background check anywhere in this crate, only this row. The request
    // never touches this thread: clicking spawns a dedicated `std::thread`
    // that calls `update_check::check_for_update` and reports back over a
    // fresh `crossbeam_channel`.
    //
    // The row stays enabled (and re-clickable) once a check is done, both
    // to let the user retry after a transient network error and to let
    // them re-check right before upgrading; it is disabled only while one
    // is already in flight, so a click can't pile up a second thread
    // racing the first. Issue #250 extends that to an in-progress install
    // and to `Restarting`, where there is nothing left to check.
    let busy = matches!(
        update_check,
        UpdateCheckState::Checking { .. }
            | UpdateCheckState::Installing { .. }
            | UpdateCheckState::Restarting
    );
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Check for updates",
            trailing: Trailing::None,
            enabled: !busy,
        },
    )
    .clicked()
    {
        *update_check = start_update_check();
    }
    // Issue #250: an "Update now" click can't assign `*update_check` from
    // inside the match below, which borrows it — so the click is collected
    // here and acted on once the match has ended.
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
            // The install thread reports once, at the end — WinHTTP's read
            // loop has no progress callback wired through
            // `platform::http_get_bytes` — so this is a spinner, not a
            // percentage. Claiming a percentage it cannot know would be
            // worse than not showing one.
            ui.spinner();
        }
        UpdateCheckState::Restarting => {
            ui.label("Restarting…");
        }
        UpdateCheckState::InstallFailed { available, error } => {
            // The offer is redrawn above the error on purpose: a failed
            // download is usually transient (a dropped connection, a proxy
            // hiccup), so the retry has to be one click away rather than
            // behind a fresh check.
            draw_update_available(ui, available, &mut clicked_install);
            ui.label(format!("Update failed: {error}"));
        }
    }
    if let Some(available) = clicked_install {
        *update_check = start_update_install(available);
    }

    // Issue #203: a UI-settings reset (window size + opacity), distinct
    // from the tray's own OS-level `TrayCommand::ResetWindow` and from the
    // header toggle cluster's Reset *icon*, which resets encounter data
    // (`UiCommand::Reset`) and touches no settings at all — issue #121 is
    // explicit that the two must not be confused. Issue #121 also widened
    // what "defaults" means: `Settings::reset_to_defaults` covers the whole
    // struct, and the image cache is dropped alongside it so textures for
    // images that are no longer configured are released the same frame.
    if menu_row(
        ui,
        MenuRow {
            icon: icons.toolbar.get(ToolbarIcon::Reset),
            label: "Reset to defaults",
            trailing: Trailing::None,
            enabled: true,
        },
    )
    .clicked()
    {
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

    // Issue #53: this minimize goes to the notification area, not the
    // taskbar. `platform::install_tray`'s subclass intercepts the
    // `WM_SIZE`/`SIZE_MINIMIZED` this command produces, adds a tray icon
    // and hides the window — but the tray icon is now the *only* way back,
    // so don't route this through anything that bypasses a real minimize.
    if menu_row(
        ui,
        MenuRow {
            icon: None,
            label: "Minimize to tray",
            trailing: Trailing::None,
            enabled: true,
        },
    )
    .clicked()
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ui.close();
    }

    if menu_row(
        ui,
        MenuRow {
            icon: icons.toolbar.get(ToolbarIcon::Close),
            label: "Close",
            trailing: Trailing::None,
            enabled: true,
        },
    )
    .clicked()
    {
        let _ = tx_command.try_send(UiCommand::Quit);
        *quit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        ui.close();
    }

    nav
}

/// The Columns page (issue #120): the same per-column checkboxes issue
/// #13 introduced, grouped by `column_groups` and annotated.
///
/// Two things are visible here that were not before. Each row carries a
/// `ColumnKind::sample_text` so the user can see what a column actually
/// puts in a row without enabling it; and a column `Settings::toggle`
/// would refuse to disable is greyed out with `column_lock_hint` on its
/// disabled-hover tooltip, plus one footer line repeating the reason —
/// previously such a click silently did nothing at all.
///
/// The back row shares its line with a column-scoped "Reset" (see
/// `reset_columns_to_default`), enabled only when the set actually
/// differs from the default.
/// Whether the Columns page's Reset button should be enabled: `true` iff
/// some column's visibility actually differs from `Settings::default`.
///
/// Order-insensitive by construction: `Settings::toggle` re-enabling a
/// column pushes it to the end of `visible_columns` rather than restoring
/// its original slot, so a plain `Vec` comparison against the default
/// would leave Reset lit up after an off-then-on toggle that changed
/// nothing about which columns are actually visible.
fn columns_differ_from_default(settings: &Settings) -> bool {
    let default_settings = Settings::default();
    ColumnKind::ALL
        .iter()
        .any(|col| settings.is_visible(*col) != default_settings.is_visible(*col))
}

fn draw_columns_page(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
) -> Option<MenuNav> {
    let mut nav = None;
    let mut changed = false;

    let reset_enabled = columns_differ_from_default(settings);
    ui.horizontal(|ui| {
        ui.set_min_height(MENU_ROW_HEIGHT);
        let back_width = (ui.available_width() - MENU_COLUMNS_RESET_WIDTH).max(1.0);
        if menu_back_row(ui, "Columns", back_width).clicked() {
            nav = Some(MenuNav::Back);
        }
        if ui
            .add_enabled(reset_enabled, egui::Button::new("Reset").small())
            .clicked()
        {
            reset_columns_to_default(settings);
            changed = true;
        }
    });

    // The first lock found drives the footer: with at most two guards and
    // a single locked column in practice, repeating one line per locked
    // row would be noise.
    let mut locked: Option<ColumnLock> = None;
    for group in column_groups() {
        menu_section(ui, group.title);
        menu_hint(ui, group.hint);
        for &col in group.columns {
            let lock = column_toggle_lock(settings, col);
            if locked.is_none() {
                locked = lock;
            }
            let mut checked = settings.is_visible(col);
            let toggled = ui
                .horizontal(|ui| {
                    ui.set_min_height(MENU_ROW_HEIGHT);
                    ui.add_space(MENU_ROW_INSET);
                    let mut response = ui.add_enabled(
                        lock.is_none(),
                        egui::Checkbox::new(&mut checked, col.label()),
                    );
                    if let Some(lock) = lock {
                        response = response.on_disabled_hover_text(column_lock_hint(lock));
                    }
                    let sample_color = ui.visuals().weak_text_color();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(MENU_ROW_INSET);
                        ui.label(
                            egui::RichText::new(col.sample_text())
                                .monospace()
                                .size(MENU_TRAILING_FONT_SIZE)
                                .color(sample_color),
                        );
                    });
                    response.changed()
                })
                .inner;
            if toggled {
                settings.toggle(col);
                changed = true;
            }
        }
    }
    // Re-derive the reason after any in-frame toggle above: a checkbox
    // that unlocked or locked a sibling this frame must have its footer
    // reflect the post-toggle state, not the reason latched while
    // iterating groups.
    if changed {
        locked = None;
        for group in column_groups() {
            for &col in group.columns {
                if let Some(lock) = column_toggle_lock(settings, col) {
                    locked = Some(lock);
                    break;
                }
            }
            if locked.is_some() {
                break;
            }
        }
    }
    if let Some(lock) = locked {
        menu_hint(ui, column_lock_hint(lock));
    }

    if changed {
        // Same persistence path as the opacity slider: blocking file IO
        // stays off this render thread, and a dropped writer thread just
        // leaves the in-memory value correct for the rest of the session.
        let _ = tx_settings.send(settings.clone());
    }
    nav
}

/// The Background images page (issues #121, #253, #120): one section per
/// `ImageSlot`, each with `background_image_row`'s Choose/Clear pair and
/// its status line. Only the framing moved — the row's own logic, cache
/// invalidation and error reporting are unchanged.
fn draw_backgrounds_page(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
    icons: &Icons,
) -> Option<MenuNav> {
    let mut nav = None;
    let back_width = ui.available_width();
    if menu_back_row(ui, "Background images", back_width).clicked() {
        nav = Some(MenuNav::Back);
    }
    for slot in ImageSlot::ALL {
        menu_section(ui, slot.label());
        background_image_row(ui, slot, settings, tx_settings, icons);
    }
    nav
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;

    // -- issue #120: drill-down pages, row layout, column grouping --------

    /// The root page is exactly the ten rows of the redesign, in order,
    /// under its three section labels — and none of the column
    /// checkboxes, which now live a click away on their own page.
    ///
    /// Reads the rows back out of the AccessKit tree sorted by their
    /// painted `y`, so this pins the order the user actually sees rather
    /// than the order the source happens to call `menu_row` in. The
    /// section labels are painted text, not widgets, so they are checked
    /// through `header_menu_texts` instead.
    #[test]
    fn the_root_page_carries_exactly_its_ten_rows_in_order() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_header_menu(
                ui,
                &ctx,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                &mut false,
            );
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        // The opacity `Slider` is a labelled widget too, but
        // `show_value(false)` leaves its name empty — it is the rail under
        // the "Opacity" row, not a row of its own, so it drops out here.
        let mut rows: Vec<(f32, String)> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| {
                let label = node.label().map(str::to_owned)?;
                let bounds = node.bounds()?;
                (!label.is_empty()).then_some((bounds.y0 as f32, label))
            })
            .collect();
        output.drop_without_applying_deltas();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let names: Vec<String> = rows.into_iter().map(|(_, name)| name).collect();

        assert_eq!(
            names,
            vec![
                "Columns",
                "Opacity",
                "Background images",
                "Restart packet capture",
                "Export logs…",
                "Export session bundle…",
                "Check for updates",
                "Reset to defaults",
                "Minimize to tray",
                "Close",
            ]
        );

        let texts = header_menu_texts(UpdateCheckState::Idle);
        for section in ["DISPLAY", "SESSION", "APP"] {
            assert!(
                texts.contains(&section.to_string()),
                "expected the {section} section label among the painted text, got {texts:?}"
            );
        }
        for col in ColumnKind::ALL {
            assert!(
                !texts.contains(&col.label().to_string()),
                "{:?}'s checkbox must not paint on the root page: {texts:?}",
                col
            );
        }
    }

    /// `Back` is unconditional: whichever page the user is on, it lands
    /// back on the root, so the back row never needs per-page wiring.
    #[test]
    fn next_page_opens_a_page_and_back_always_returns_to_root() {
        assert_eq!(
            next_page(MenuNav::Open(MenuPage::Columns)),
            MenuPage::Columns
        );
        assert_eq!(
            next_page(MenuNav::Open(MenuPage::Backgrounds)),
            MenuPage::Backgrounds
        );
        assert_eq!(next_page(MenuNav::Back), MenuPage::Root);
        assert_eq!(MenuPage::default(), MenuPage::Root);
    }

    /// The label's left edge is a function of the *slot*, never of whether
    /// this particular row happens to carry an icon — `menu_row_layout`
    /// takes no icon argument at all, which is what makes an icon-less row
    /// line up with an icon-bearing one directly above it.
    #[test]
    fn menu_row_layout_reserves_the_icon_slot_and_right_aligns_the_trailing() {
        let row = egui::Rect::from_min_size(egui::pos2(10.0, 4.0), egui::vec2(248.0, 24.0));
        let rects = menu_row_layout(row, MENU_ROW_INSET, MENU_ICON_SLOT, 40.0);

        assert_eq!(rects.icon.left(), row.left() + MENU_ROW_INSET);
        assert_eq!(rects.icon.width(), MENU_ICON_SLOT);
        assert_eq!(rects.icon.center().y, row.center().y);
        assert_eq!(rects.label.left(), rects.icon.right() + MENU_ROW_LABEL_GAP);
        assert_eq!(rects.trailing.right(), row.right() - MENU_ROW_INSET);
        assert_eq!(rects.trailing.width(), 40.0);
        assert!(
            rects.label.right() <= rects.trailing.left(),
            "label {:?} must not run under the trailing {:?}",
            rects.label,
            rects.trailing
        );
    }

    /// A row with nothing trailing still gets a rect — an empty one pinned
    /// to the right content edge — rather than an inverted or absent one,
    /// so callers can paint into it unconditionally.
    #[test]
    fn menu_row_layout_puts_an_empty_trailing_at_the_right_edge() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(248.0, 24.0));
        let rects = menu_row_layout(row, MENU_ROW_INSET, MENU_ICON_SLOT, 0.0);
        assert_eq!(rects.trailing.width(), 0.0);
        assert_eq!(rects.trailing.left(), row.right() - MENU_ROW_INSET);
    }

    /// A hopelessly narrow row must degrade to non-inverted rects, the
    /// same guarantee `chevron_rect`/`title_toggle_pill_rect` make.
    #[test]
    fn menu_row_layout_never_inverts_in_an_absurdly_narrow_row() {
        let row = egui::Rect::from_min_size(egui::pos2(3.0, 1.0), egui::vec2(6.0, 24.0));
        let rects = menu_row_layout(row, MENU_ROW_INSET, MENU_ICON_SLOT, 40.0);
        for rect in [rects.icon, rects.label, rects.trailing] {
            assert!(rect.width() >= 0.0, "{rect:?} is inverted");
        }
    }

    /// The Columns page's grouping must partition `ColumnKind::ALL`: every
    /// column reachable, none listed twice, and each group's own order a
    /// subsequence of the canonical left-to-right order so the page reads
    /// the same way the table does.
    #[test]
    fn column_groups_cover_every_column_exactly_once_in_canonical_order() {
        let mut seen: Vec<ColumnKind> = Vec::new();
        for group in column_groups() {
            assert!(!group.title.is_empty(), "a group needs a title");
            assert!(!group.hint.is_empty(), "{} needs a hint", group.title);
            let mut previous: Option<usize> = None;
            for col in group.columns {
                let index = ColumnKind::ALL
                    .iter()
                    .position(|c| c == col)
                    .expect("a group may only list real columns");
                if let Some(previous) = previous {
                    assert!(
                        index > previous,
                        "{}: {col:?} breaks canonical order",
                        group.title
                    );
                }
                previous = Some(index);
                seen.push(*col);
            }
        }
        assert_eq!(seen.len(), ColumnKind::ALL.len(), "{seen:?}");
        for col in ColumnKind::ALL {
            assert_eq!(
                seen.iter().filter(|c| **c == col).count(),
                1,
                "{col:?} must appear in exactly one group: {seen:?}"
            );
        }
    }

    /// The default column set is comfortably clear of both guards, so a
    /// fresh install sees no greyed-out checkbox at all.
    #[test]
    fn column_toggle_lock_leaves_the_default_set_fully_editable() {
        let settings = Settings::default();
        for col in ColumnKind::ALL {
            assert_eq!(
                column_toggle_lock(&settings, col),
                None,
                "{col:?} must not be locked in the default set"
            );
        }
    }

    /// `Settings::toggle`'s first guard, made visible: the one remaining
    /// visible column cannot be switched off.
    #[test]
    fn column_toggle_lock_locks_the_last_visible_column() {
        let settings = Settings {
            visible_columns: vec![ColumnKind::Dps],
            ..Default::default()
        };
        assert_eq!(
            column_toggle_lock(&settings, ColumnKind::Dps),
            Some(ColumnLock::LastVisible)
        );
    }

    /// `Settings::toggle`'s second guard (issue #168): the inline
    /// name-suffix columns don't occupy a stat slot, so the last column
    /// that *does* is locked — while the inline one beside it stays free.
    #[test]
    fn column_toggle_lock_locks_the_last_stat_column_but_not_an_inline_one() {
        let settings = Settings {
            visible_columns: vec![ColumnKind::AbilityScore, ColumnKind::Dps],
            ..Default::default()
        };
        assert_eq!(
            column_toggle_lock(&settings, ColumnKind::Dps),
            Some(ColumnLock::LastStatColumn)
        );
        assert_eq!(
            column_toggle_lock(&settings, ColumnKind::AbilityScore),
            None
        );
    }

    /// A hidden column is always free to turn *on*, whatever the rest of
    /// the set looks like — the guards are about removals only.
    #[test]
    fn column_toggle_lock_never_locks_a_hidden_column() {
        let settings = Settings {
            visible_columns: vec![ColumnKind::Dps],
            ..Default::default()
        };
        for col in ColumnKind::ALL {
            if col == ColumnKind::Dps {
                continue;
            }
            assert_eq!(column_toggle_lock(&settings, col), None, "{col:?}");
        }
    }

    /// User-facing text: the hint has to name *which* guard is holding the
    /// checkbox, or a greyed-out row just reads as a bug.
    #[test]
    fn column_lock_hint_names_the_guard() {
        assert_eq!(
            column_lock_hint(ColumnLock::LastVisible),
            "At least one column must stay visible"
        );
        assert_eq!(
            column_lock_hint(ColumnLock::LastStatColumn),
            "At least one stat column must stay visible"
        );
    }

    #[test]
    fn visible_count_text_counts_against_every_column() {
        assert_eq!(visible_count_text(&Settings::default()), "4 of 9");
        let settings = Settings {
            visible_columns: ColumnKind::ALL.to_vec(),
            ..Default::default()
        };
        assert_eq!(visible_count_text(&settings), "9 of 9");
    }

    /// Rounded, not truncated: 0.799 has to read as 80%, not 79%.
    #[test]
    fn opacity_percent_text_rounds_to_a_whole_percent() {
        assert_eq!(opacity_percent_text(0.8), "80%");
        assert_eq!(opacity_percent_text(1.0), "100%");
        assert_eq!(opacity_percent_text(0.0), "0%");
        assert_eq!(opacity_percent_text(0.799), "80%");
    }

    #[test]
    fn backgrounds_summary_text_counts_the_configured_slots() {
        let mut settings = Settings::default();
        assert_eq!(backgrounds_summary_text(&settings), "none");
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("a.png")));
        assert_eq!(backgrounds_summary_text(&settings), "1 set");
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("b.png")));
        assert_eq!(backgrounds_summary_text(&settings), "2 set");
    }

    /// The Columns page's own Reset is column-scoped — unlike the root
    /// page's "Reset to defaults", it must not touch opacity, the window
    /// toggles, or the history retention fields.
    #[test]
    fn reset_columns_to_default_touches_only_the_column_set() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Hits],
            opacity: 0.25,
            always_on_top: false,
            click_through: true,
            history_max_encounters: 7,
            ..Default::default()
        };

        reset_columns_to_default(&mut settings);

        assert_eq!(
            settings.visible_columns,
            Settings::default().visible_columns
        );
        assert_eq!(settings.opacity, 0.25);
        assert!(!settings.always_on_top);
        assert!(settings.click_through);
        assert_eq!(settings.history_max_encounters, 7);
    }

    /// `columns_differ_from_default` drives the Reset button's enabled
    /// state, so it must come back down once a toggle is undone — not
    /// just go up when one is made.
    #[test]
    fn columns_differ_from_default_ignores_an_off_then_on_toggle() {
        let mut settings = Settings::default();
        let col = *ColumnKind::ALL
            .iter()
            .find(|col| settings.is_visible(**col))
            .expect("default settings show at least one column");

        settings.toggle(col);
        settings.toggle(col);
        assert!(!columns_differ_from_default(&settings));

        settings.toggle(col);
        assert!(columns_differ_from_default(&settings));
    }

    /// Every column's trailing sample has to say something — an empty one
    /// would leave a ragged hole in the page's right-hand column.
    #[test]
    fn every_column_has_a_non_empty_sample_text() {
        for col in ColumnKind::ALL {
            assert!(!col.sample_text().is_empty(), "{col:?}");
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

    /// `menu_row` paints its own label rather than delegating to a
    /// `Button` atom, so — unlike the `menu_item_button` it replaced — the
    /// accessible name only exists because of the explicit `widget_info`
    /// call. Without it a screen-reader user hears an unlabeled control
    /// and every header test loses the only handle it has on a row.
    /// A row keeps that name whether or not its icon texture decoded, and
    /// its height is the fixed `MENU_ROW_HEIGHT` either way.
    #[test]
    fn menu_row_has_an_accessible_label_with_or_without_an_icon() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let texture = ctx.load_texture(
            "test-icon",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );

        for icon in [None, Some(&texture)] {
            let mut id = egui::Id::NULL;
            let mut rect = egui::Rect::ZERO;
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let response = menu_row(
                    ui,
                    MenuRow {
                        icon,
                        label: "Close",
                        trailing: Trailing::None,
                        enabled: true,
                    },
                );
                id = response.id;
                rect = response.rect;
            });
            let update = output
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            let label = accessible_label(&update, id);
            output.drop_without_applying_deltas();

            assert_eq!(label.as_deref(), Some("Close"));
            assert_eq!(rect.height(), MENU_ROW_HEIGHT);
        }
    }

    /// An "opens a dialog" row announces itself with the same "…" it
    /// paints — the accessible name is the painted name, not the bare
    /// label the call site passed in.
    #[test]
    fn menu_row_ellipsis_rows_announce_the_ellipsis() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();

        let mut id = egui::Id::NULL;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            id = menu_row(
                ui,
                MenuRow {
                    icon: None,
                    label: "Export logs",
                    trailing: Trailing::Ellipsis,
                    enabled: true,
                },
            )
            .id;
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let label = accessible_label(&update, id);
        output.drop_without_applying_deltas();

        assert_eq!(label.as_deref(), Some("Export logs…"));
    }

    /// A disabled row must read as inert to AccessKit, not merely paint
    /// dimmer — the same guarantee `add_enabled(false, ..)` gave the
    /// buttons this widget replaced.
    #[test]
    fn menu_row_marks_a_disabled_row_disabled() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();

        let mut id = egui::Id::NULL;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            id = menu_row(
                ui,
                MenuRow {
                    icon: None,
                    label: "Check for updates",
                    trailing: Trailing::None,
                    enabled: false,
                },
            )
            .id;
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let disabled = update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())
            .map(|(_, node)| node.is_disabled());
        output.drop_without_applying_deltas();

        assert_eq!(disabled, Some(true));
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
    /// — this drives the real `draw_header` (chevron, popup wiring, and
    /// all) rather than calling `draw_header_menu` directly.
    ///
    /// Issue #120 changed what triggers the height change: the Columns
    /// list is a *page* now rather than an inline disclosure, so the popup
    /// swaps one body for a differently-sized one instead of growing in
    /// place. The feedback loop is identical either way — with the fix
    /// removed, the `Area`'s remembered root-page rect keeps being handed
    /// back as this `Ui`'s `max_rect` and the popup stays frozen at the
    /// root page's height no matter how many frames pass — so the test
    /// asserts the popup actually re-measures for the new page, and that
    /// it stays inside the cap on both.
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
    /// one short enough to force scrolling): the failure this guards is
    /// "stuck at the previous page's height", not "overflowed the cap",
    /// and a generous screen isolates it from the cap — which this test
    /// also checks holds on every frame regardless.
    #[test]
    fn header_menu_popup_remeasures_when_the_columns_page_opens() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        // A zero animation time makes the page swap land whole on the
        // frame after the click instead of fading in over several.
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(0);
        let mut gesture = WindowGesture::default();
        let mut update_check = UpdateCheckState::default();

        // Tall enough that both pages fit with room to spare — see the doc
        // comment above for why a generous screen, not a short one, is
        // what isolates this regression.
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
        // the just-opened `Area` runs a sizing-only pass — so this only
        // checks that it opened at all, via a root-page row.
        let update = frame(click_at(chevron_pos));
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label().is_some_and(|s| s == "Close")),
            "clicking the chevron must open the menu"
        );

        // Frame 3: let the just-opened popup settle out of its first,
        // sizing-only pass into a stable position.
        let update = frame(egui::RawInput::default());
        let columns_pos = accessible_rect_for_label(&update, "Columns").center();
        let root_height = popup_height(popup_id);
        assert!(
            root_height <= scroll_max_height,
            "the root page's height {root_height} must already be within the \
             {scroll_max_height} cap"
        );

        // Frame 4: open the Columns page — a whole new body, sized by nine
        // checkboxes, three group headers and their hints.
        let _ = frame(click_at(columns_pos));

        // Several more settle frames with no further input: this is
        // exactly where issue #231's feedback loop lived. Without
        // `ui.set_max_height`, the `Area`'s remembered root-page rect kept
        // being handed straight back to this `Ui` as its `max_rect` on
        // every one of these frames, and the swapped-in page's real height
        // never had a chance to register.
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
        // pinned at `root_height` instead of taking the Columns page's own
        // size.
        let page_height = *heights.last().unwrap();
        assert!(
            (page_height - root_height).abs() > 5.0,
            "opening the Columns page must re-measure the popup away from the \
             root page's height {root_height}, got {heights:?}"
        );
        // And it must settle there promptly, not keep climbing frame after
        // frame once the popup has had several frames to settle.
        let peak = heights.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            page_height >= peak - 1.0,
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
