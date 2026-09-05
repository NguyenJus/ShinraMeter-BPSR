//! Player table: rows, columns, share bars, and row icons.

use super::*;

/// Takes the whole `Icons` bundle rather than just `ClassIcons`: since issue
/// #49 a row paints a toolbar-set texture too (the death counter's skull),
/// and handing both sets down as one argument keeps `draw_row` from growing
/// a second icon parameter.
/// Issue #84's shrink-vs-scroll decision, as a pure function of the
/// viewport width and the enabled stat columns' combined width — extracted
/// so the floor is unit-testable without a live `Ui`/`ScrollArea`, the same
/// reasoning `column_anchors`, `share_bar_paints` etc. already follow.
///
/// `column_anchors`'s own `scale` factor already shrinks every column
/// proportionally when the row is narrower than the columns' combined
/// width — that stays the *first* response to a narrow window, so mild
/// narrowing still looks exactly like it did before this issue. But that
/// shrink had no floor of its own, so a badly narrow window used to
/// compress column text into illegibility. `MIN_COLUMN_SCALE` gives it one:
/// the returned width is what row content is actually laid out at, and it
/// never drops below the width at which `column_anchors` would shrink
/// columns past that floor (i.e. below `stat_columns_total *
/// MIN_COLUMN_SCALE`, plus `COLUMN_RIGHT_MARGIN` since `column_anchors`
/// reserves that margin off the right edge before it ever sees a column
/// width). Once the viewport is narrower than that floor, the returned
/// width stays pinned at the floor — wider than the viewport — and the
/// caller's `ScrollArea` picks up the remaining overflow as horizontal
/// scroll instead of compressing columns any further.
pub(crate) fn row_content_width(viewport_width: f32, stat_columns_total: f32) -> f32 {
    let floor_width = stat_columns_total * MIN_COLUMN_SCALE + COLUMN_RIGHT_MARGIN;
    viewport_width.max(floor_width)
}

/// Returns the `ScrollArea`'s reported content size — larger than the
/// viewport on whichever axis actually needed to scroll this frame — purely
/// so tests can observe that without reaching into egui's persisted scroll
/// state; `OverlayApp::ui` (the only production caller) ignores it.
pub(crate) fn draw_rows(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    settings: &Settings,
    icons: &Icons,
    // Issue #16 (D1): set to `Some(uid)` when `draw_row` reports a
    // right-click this frame. A plain out-parameter rather than a return
    // value, since `draw_rows`' own return (the `ScrollArea` content size)
    // is already spoken for and belongs to a different caller (the
    // screenshot-crop bound).
    opened: &mut Option<i64>,
) -> egui::Vec2 {
    // `Settings::stat_columns`, not `ordered_columns` (issue #168):
    // `AbilityScore`/`SeasonStrength`, even when enabled, must not reserve
    // stat-column width or an anchor here — they render inline with the
    // name instead (`draw_row`'s `name_suffix` call). The enabled-column
    // set (and therefore the column widths) is identical for every row in
    // a frame, so it's computed once here rather than once per row inside
    // `draw_row`.
    let columns = settings.stat_columns();
    let stat_columns = stat_columns_for(&columns);
    let stat_columns_total: f32 = stat_columns.iter().map(|c| c.width).sum();
    let content_width = row_content_width(ui.available_width(), stat_columns_total);

    let output = egui::ScrollArea::both()
        // Same footprint as a plain, unwrapped layout: the scroll area
        // always fills the space `CentralPanel` gives it rather than
        // shrinking to the content's size, so wrapping rows in it changes
        // nothing when everything already fits.
        .auto_shrink([false, false])
        // Hidden entirely — no reserved gutter, no visible track — unless
        // the content actually overflows, so a window sized to show every
        // row and every column at full width paints pixel-identically to
        // today's unscrolled layout. `apply_theme` never touches
        // `style.spacing.scroll`, so this also inherits egui's default
        // `ScrollStyle::floating()`: a thin, translucent bar that only
        // fades in on hover and reserves no layout space even while
        // visible — already the unobtrusive styling the row list needs,
        // with nothing further to configure here.
        .scroll_bar_visibility(
            egui::containers::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
        )
        .show(ui, |ui| {
            // True contiguous 30pt rows (decision 3): scoped to the scroll
            // area's own content `Ui`, so the header and menus keep
            // `apply_theme`'s `item_spacing` — rows' hover bands and accent
            // lines must sit flush against their neighbors with no gap,
            // which a nonzero `item_spacing.y` would reintroduce.
            ui.spacing_mut().item_spacing.y = 0.0;
            // A horizontal `ScrollArea` measures how much there is to
            // scroll from what its content `Ui` actually ends up using; an
            // empty `snapshot.rows` would otherwise paint nothing and
            // report zero content width even when `content_width` (the
            // floor above) exceeds the viewport.
            ui.set_min_width(content_width);

            let avail = ui.available_rect_before_wrap();
            let anchors = column_anchors(
                avail.left(),
                avail.left() + content_width,
                &stat_columns,
                COLUMN_RIGHT_MARGIN,
            );
            let layout = RowLayout {
                kinds: &columns,
                columns: &stat_columns,
                anchors: &anchors,
                settings,
            };

            // Issue #73: each row's damage-share bar is now scaled to the
            // *top row's* damage rather than to its share of total
            // encounter damage, so the highest-damage row always fills the
            // full bar width. `snapshot.rows` is already sorted descending
            // by damage (`Encounter::snapshot`), so the top row's damage is
            // just the first row's, computed once here rather than once
            // per row inside `draw_row`.
            let top_damage = snapshot.rows.first().map(|r| r.damage).unwrap_or(0);

            for row in &snapshot.rows {
                if let Some(uid) = draw_row(ui, row, &layout, icons, top_damage, content_width) {
                    *opened = Some(uid);
                }
            }
        });
    output.content_size
}

/// How prominently one stat column's text is painted (issue #56). The
/// reference render is not flat: a row's DPS value is its headline number,
/// the percentages beside it are supporting color, and everything else sits
/// between the two. Derived from the `ColumnKind` rather than carried on
/// `StatColumn`, because `StatColumn` is built in `settings.rs`
/// (`ColumnKind::spec`) and typography belongs with the painting, not with
/// the column's width and formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnEmphasis {
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
    pub(crate) fn font(self) -> egui::FontId {
        match self {
            Self::Value | Self::Stat | Self::Percent => regular(FONT_SIZE_ROW),
            Self::Counter => bold(FONT_SIZE_COUNTER),
        }
    }

    /// Whether `draw_row` paints this column as a pill rather than as text.
    pub(crate) fn is_pill(self) -> bool {
        matches!(self, Self::Counter)
    }
}

/// Maps a column to its emphasis level (issue #56).
pub(crate) fn column_emphasis(kind: ColumnKind) -> ColumnEmphasis {
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
pub(crate) fn stat_columns_for(columns: &[ColumnKind]) -> Vec<StatColumn> {
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
    let widths: Vec<f32> = columns.iter().map(|c| c.width).collect();
    column_anchors_from_widths(rect_left, rect_right, &widths, margin)
}

/// Same anchor placement as [`column_anchors`], over bare widths rather
/// than full `StatColumn`s — for a caller (`draw_skill_window`) that has a
/// fixed width per column but no per-row `text`/`color` to paint through a
/// `StatColumn`.
pub fn column_anchors_from_widths(
    rect_left: f32,
    rect_right: f32,
    widths: &[f32],
    margin: f32,
) -> Vec<f32> {
    let total_width: f32 = widths.iter().sum();
    let available = (rect_right - rect_left - margin).max(0.0);
    let scale = if total_width > available && total_width > 0.0 {
        available / total_width
    } else {
        1.0
    };

    let mut anchors = Vec::with_capacity(widths.len());
    let mut x = rect_right - margin;
    for &width in widths.iter().rev() {
        anchors.push(x);
        x -= width * scale;
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
/// This is what keeps an out-of-range value (e.g. a packet-decoded stat
/// past the in-game ceiling `StatColumn`'s `width` budget assumes — see
/// `ColumnKind::spec`) from painting arbitrarily far left across the row:
/// `draw_row` clips every stat column's text draw to this rect rather than
/// trusting the formatted string to fit `width`, so an overlong string
/// loses its leading glyphs after one column's worth instead of running
/// over its neighbors.
///
/// Its reach is exactly the columns that occupy a stat slot — the ones
/// `Settings::stat_columns` hands `draw_rows`. `AbilityScore`/
/// `SeasonStrength` are no longer among them while enabled (issue #168):
/// they leave the grid entirely and reach the screen only through
/// `draw_row`'s inline name suffix (`name_suffix`), which is deliberately
/// unclipped and uncapped, so an over-ceiling value there widens the suffix
/// and bleeds *under* the stat columns instead of losing glyphs. The
/// z-order reasoning that makes that safe lives at that paint site.
///
/// The slot is the *nominal* `width`, deliberately not the gap between this
/// column's anchor and the previous one. The two are identical whenever the
/// row is wide enough to hold every column at full width; in a narrower row
/// `column_anchors` scales those gaps down (see there), and clipping to a
/// scaled gap would cut ordinary in-range values short — right-aligned text
/// is clipped from the *left*, so a clipped `99.99K` reads as a smaller
/// number rather than as damage. In that compressed case the slots overlap
/// and an overflowing value can bleed into its neighbor, which is exactly
/// what a too-narrow window did before any clipping existed; visible
/// overlap beats silently hiding digits. So the clip is a no-op only for
/// values that fit their budget in a row wide enough not to be compressed.
///
/// `Painter::with_clip_rect` intersects with the parent's clip rect, so a
/// slot reaching past the row's own left edge is still bounded by the ui.
pub(crate) fn column_clip_rect(rect: egui::Rect, anchor: f32, width: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(anchor - width, rect.top()),
        egui::pos2(anchor, rect.bottom()),
    )
}

/// Row layout that's identical across every row in a frame — the enabled
/// column kinds, their `StatColumn` specs, and the anchors `column_anchors`
/// derived from them — computed once by `draw_rows` and handed to every
/// `draw_row` call rather than recomputed per row. Bundled into one struct
/// for the same reason `SettingsHandle` is (issue #71): three arguments
/// that are always needed together and always travel together push
/// `draw_row` over clippy's argument limit on their own, especially once
/// issue #84 adds `row_width`.
pub(crate) struct RowLayout<'a> {
    pub(crate) kinds: &'a [ColumnKind],
    pub(crate) columns: &'a [StatColumn],
    pub(crate) anchors: &'a [f32],
    /// Issue #168: `draw_row` needs the live `Settings` (not just `kinds`,
    /// which now excludes `AbilityScore`/`SeasonStrength` — see
    /// `Settings::stat_columns`) to compose the name-suffix text via
    /// `name_suffix`. Bundled into `RowLayout` alongside the three fields
    /// above for the same reason they are: keeps `draw_row`'s own argument
    /// count under clippy's limit rather than adding a fourth loose
    /// parameter.
    pub(crate) settings: &'a Settings,
}

pub(crate) fn draw_row(
    ui: &mut egui::Ui,
    row: &PlayerRow,
    layout: &RowLayout<'_>,
    icons: &Icons,
    top_damage: i64,
    // Issue #84: the width every row is laid out at, computed once by
    // `draw_rows` (`content_width`) rather than read from
    // `ui.available_width()` here — inside a horizontal `ScrollArea` that
    // can report an unbounded width, which is not what a row should paint
    // itself at.
    row_width: f32,
    // Issue #16: `Some(row.uid)` when this row was right-clicked this
    // frame, so `draw_rows` can open (or re-show) its breakdown window.
    // `Sense::click()` (widened from `Sense::hover()`) still reports
    // `hovered()` exactly as before, so the hover gradient below is
    // unaffected; left-click stays free for the window drag, which lives
    // on the header band and resize strips, never on a row.
) -> Option<i64> {
    let desired_size = egui::vec2(row_width, ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

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

    // The icon slots (issue #9, widened by issue #33) are reserved at a
    // fixed offset regardless of whether this row's class has an icon or
    // its two Imagine slots are filled, so names stay left-aligned in a
    // column across rows either way — only the painting below is
    // conditional.
    let RowIconSlots {
        class: icon_rect,
        imagines: imagine_slots,
        name_offset,
    } = icon_slots(rect);
    if let Some(texture) = row.class.and_then(|class| icons.classes.get(class)) {
        ui.painter()
            .image(texture.id(), icon_rect, UV_FULL, CLASS_ICON_TINT);
    }

    // IMAGINE-TAKEDOWN: one of five sites — see
    // `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
    //
    // The two Imagine slots (issue #33): a filled slot paints the equipped
    // Imagine's icon (pre-masked to a circle at asset-prep time, so no
    // runtime clip is needed) with a name+tier tooltip on hover (issue
    // #169) and a gold ring at max tier (issue #170); an empty slot, an id
    // outside the curated table, or a texture that failed to decode all
    // degrade to the same blank-circle placeholder — one branch, never a
    // panic (D4's runtime-degrade path).
    for (i, slot) in imagine_slots.into_iter().enumerate() {
        let filled = row.imagines[i]
            .and_then(imagines::imagine_of_skill_id)
            .and_then(|im| icons.imagines.get(im.icon).map(|texture| (im, texture)));
        match filled {
            Some((im, texture)) => {
                let tier = row.imagine_tiers[i];
                ui.painter()
                    .image(texture.id(), slot, UV_FULL, CLASS_ICON_TINT);
                if imagine_ring_visible(tier) {
                    ui.painter().circle_stroke(
                        slot.center(),
                        IMAGINE_SIZE / 2.0,
                        egui::Stroke::new(IMAGINE_MAX_TIER_RING_WIDTH, IMAGINE_MAX_TIER_RING_COLOR),
                    );
                }
                ui.interact(
                    slot,
                    ui.id().with(("imagine", row.uid, i)),
                    egui::Sense::hover(),
                )
                .on_hover_text(imagine_hover_text(im.name, tier));
            }
            None => {
                ui.painter()
                    .circle_filled(slot.center(), IMAGINE_SIZE / 2.0, IMAGINE_SLOT_EMPTY);
            }
        }
    }

    // Regular weight, at the row's flat metric size (issue #62): the
    // source's row text carries no `FontWeight`, name included — hierarchy
    // comes from color, and the name is already the brightest thing left of
    // the stat columns at plain white.
    let name = row_name(row);
    let name_rect = paint_text(
        ui.painter(),
        rect.left_center() + egui::vec2(name_offset, 0.0),
        egui::Align2::LEFT_CENTER,
        &name,
        regular(FONT_SIZE_ROW),
        egui::Color32::WHITE,
        false,
    );
    // Issue #168: `AbilityScore`/`SeasonStrength`, when enabled, paint as a
    // bracketed suffix immediately after the name rather than as their own
    // stat columns — `layout.kinds` already excludes both
    // (`Settings::stat_columns`), so this is the one place their value
    // reaches the screen. Painted from `name_rect.right_center()` (the
    // real painted extent `paint_text` just handed back, not a fixed
    // offset), so the suffix always starts flush against the name however
    // wide it rendered. Dimmed relative to the name's own opaque white
    // (`NAME_SUFFIX_ALPHA`) so it reads as secondary metadata trailing the
    // name, not as part of the name itself.
    //
    // Deliberately unclipped and never truncated/elided (issue #168
    // follow-up decision), same as the plain name text above it: a long
    // name plus both scores is allowed to visually run into the stat
    // columns rather than lose characters. That is safe specifically
    // because this paint happens *before* the stat-column loop below —
    // egui paints shapes in call order, so the columns' own paints land
    // on top of whatever the suffix already put down, and the row always
    // reads correctly at the columns' fixed anchors no matter how far the
    // suffix bleeds under them. Do not add a clip rect or a length cap
    // here — that would be undoing this decision, not fixing an oversight.
    if let Some(suffix) = name_suffix(row, layout.settings) {
        paint_text(
            ui.painter(),
            name_rect.right_center(),
            egui::Align2::LEFT_CENTER,
            &format!(" {suffix}"),
            regular(FONT_SIZE_ROW_SUFFIX),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, NAME_SUFFIX_ALPHA),
            false,
        );
    }

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

    for ((anchor_x, column), kind) in layout.anchors.iter().zip(layout.columns).zip(layout.kinds) {
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

    // Issue #16 (D1): right-click opens/re-shows this player's skill
    // breakdown; left-click is deliberately not sensed here at all — it
    // stays free for the window drag (`WindowGesture`'s header band and
    // resize strips are the only primary-button drag surfaces).
    response.secondary_clicked().then_some(row.uid)
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
pub(crate) fn paint_counter_pill(
    painter: &egui::Painter,
    row: egui::Rect,
    anchor: f32,
    pill: StatPill<'_>,
) {
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
pub(crate) fn counter_pill_rect(row: egui::Rect, anchor: f32, size: egui::Vec2) -> egui::Rect {
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
pub(crate) const SHARE_BAR_RGB_HEALER: (u8, u8, u8) = (131, 196, 154);
pub(crate) const SHARE_BAR_RGB_TANK: (u8, u8, u8) = (104, 166, 205);
pub(crate) const SHARE_BAR_RGB_DAMAGE: (u8, u8, u8) = (219, 135, 135);
/// Fallback for `Class::Unknown` (or a row with no `Class` at all). A
/// desaturated grey rather than any role's hue: reusing a role color here
/// (as this once did with `SHARE_BAR_RGB_TANK`'s blue) would make an
/// unclassified row indistinguishable from a confirmed row of that role, so
/// this must stay visually distinct from all three colors above (issue #44's
/// second open question).
pub(crate) const SHARE_BAR_RGB_UNKNOWN: (u8, u8, u8) = (140, 140, 140);

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
pub(crate) const SHARE_BAR_FILL_BOTTOM_ALPHA: u8 = 46;

/// Alpha at the *left* end of the accent line's horizontal gradient — the
/// source's `Opacity=".1"` left stop. The right stop is fully opaque (255).
pub(crate) const SHARE_BAR_ACCENT_LEFT_ALPHA: u8 = 26;

/// Thickness of the accent line along the row's bottom edge (issue #43;
/// source `Height="2"`). `share_bar_paints` clamps this against the row
/// height so it stays sane — never taller than the row itself — at small
/// row heights.
pub(crate) const SHARE_BAR_ACCENT_THICKNESS: f32 = 2.0;

/// A two-triangle gradient quad. egui has no gradient brush, so the
/// source's `LinearGradientBrush`es are reproduced as meshes with
/// per-vertex colors — exact, one draw call, and cheaper than the strip-
/// stacking `title_separator_segments` uses.
pub(crate) fn gradient_mesh(
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

pub(crate) fn vertical_gradient_mesh(
    rect: egui::Rect,
    top: egui::Color32,
    bottom: egui::Color32,
) -> egui::Mesh {
    gradient_mesh(rect, top, top, bottom, bottom)
}

pub(crate) fn horizontal_gradient_mesh(
    rect: egui::Rect,
    left: egui::Color32,
    right: egui::Color32,
) -> egui::Mesh {
    gradient_mesh(rect, left, right, left, right)
}

/// The source's per-row hover band: a horizontal gradient from transparent,
/// up to `#1fff` at 15% across, and back to transparent at the right edge —
/// a highlight that peaks near the row's left edge rather than a flat fill.
pub(crate) const ROW_HOVER_PEAK_ALPHA: u8 = 17;
pub(crate) const ROW_HOVER_PEAK_OFFSET: f32 = 0.15;

/// The two gradient quads a hovered row's highlight is made of: transparent
/// -> peak over the first `ROW_HOVER_PEAK_OFFSET` of the width, then peak ->
/// transparent over the rest. Pure, so the split point is unit-testable
/// without a live `Ui` — same reasoning as `share_bar_paints`.
pub(crate) fn row_hover_quads(rect: egui::Rect) -> [(egui::Rect, egui::Color32, egui::Color32); 2] {
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
pub(crate) struct ShareBarPaints {
    /// The share-scaled fill, vertically graded transparent -> `fill_bottom`.
    pub(crate) fill_rect: egui::Rect,
    pub(crate) fill_bottom: egui::Color32,
    /// The accent line. Issue #73: its width now matches `fill_rect`'s
    /// width exactly, so the accent underline stops exactly where the
    /// gradient fill stops rather than always spanning the full row.
    pub(crate) accent_rect: egui::Rect,
    pub(crate) accent_left: egui::Color32,
    pub(crate) accent_right: egui::Color32,
}

/// Maps a row's `Class` to its share-bar hue (issue #44). `None` — either no
/// `Class` at all or `Class::Unknown` (which has no `Role`,
/// `Class::role`) — falls back to `SHARE_BAR_RGB_UNKNOWN`, the neutral grey.
pub(crate) fn share_bar_rgb(class: Option<Class>) -> (u8, u8, u8) {
    match class.and_then(|c| c.role()) {
        Some(Role::Healer) => SHARE_BAR_RGB_HEALER,
        Some(Role::Tank) => SHARE_BAR_RGB_TANK,
        Some(Role::Damage) => SHARE_BAR_RGB_DAMAGE,
        None => SHARE_BAR_RGB_UNKNOWN,
    }
}

/// Issue #97: floor a positive-damage row's bar fraction is clamped up to,
/// so a row far below the top row (e.g. a support/healer next to a burst
/// DPS class) still paints a noticeably visible sliver instead of reading
/// as if it did zero damage. 3% sits in the middle of the sane 2-5% range:
/// at the row bar's actual width (`share_bar_rect`'s 300pt test fixture)
/// that is a solid ~9px sliver — clearly present — while staying far below
/// any real double-digit share, so it never overstates a near-zero
/// contributor as roughly comparable to a genuine one.
pub(crate) const ROW_BAR_MIN_FRAC: f32 = 0.03;

/// Computes a row's damage-share bar fraction (issue #73): `damage` scaled
/// against `top_damage` — the highest-damage row in the current snapshot —
/// rather than against total encounter damage, so the top row's bar always
/// fills the full width regardless of how dominant it is over the rest of
/// the raid. Guards against a zero or negative `top_damage` (an empty or
/// degenerate snapshot) by returning `0.0` rather than dividing.
///
/// Issue #97: any row with `damage > 0` is clamped up to at least
/// `ROW_BAR_MIN_FRAC`, so a real but tiny contributor still paints a
/// visible bar. A row with `damage == 0` (or negative, defensively) stays
/// at exactly `0.0` — no floor for a row that did nothing.
pub(crate) fn row_bar_frac(damage: i64, top_damage: i64) -> f32 {
    if top_damage <= 0 || damage <= 0 {
        0.0
    } else {
        let frac = (damage as f64 / top_damage as f64) as f32;
        frac.max(ROW_BAR_MIN_FRAC)
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
pub(crate) fn share_bar_paints(
    rect: egui::Rect,
    bar_frac: f32,
    class: Option<Class>,
) -> ShareBarPaints {
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

/// Square side of the per-row class icon (issue #9). The source's `Path
/// 18x18` was the original target; issue #187 bumps this a couple of
/// points past that on purpose, since the icon read too small in practice
/// — `ICON_MARGIN` is left unchanged, so the gutter math below no longer
/// centers this exactly in a 25px column, it just adds up.
pub(crate) const ICON_SIZE: f32 = 20.0;

/// Gap on both sides of the icon: between the row's left edge and the icon,
/// and between the icon and the Imagine gutter that follows it. `3.5` was
/// originally chosen so the class-icon portion of `ICON_GUTTER_WIDTH`
/// landed exactly on the source's 18px glyph centered in a fixed 25px
/// `SharedSizeGroup="p0"` column; issue #187 grew `ICON_SIZE` past 18
/// without touching this margin, so that exact 25px alignment no longer
/// holds — `25.0` was what `ICON_GUTTER_WIDTH` would have reverted to had
/// `IMAGINE_GUTTER_WIDTH` been deleted (D4's takedown) before issue #187.
pub(crate) const ICON_MARGIN: f32 = 3.5;

/// Class icon tint (source `Fill="#ddd"`).
pub(crate) const CLASS_ICON_TINT: egui::Color32 = egui::Color32::from_rgb(0xDD, 0xDD, 0xDD);

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Square side of each Imagine slot (issue #33) — subordinate to the
/// 20x20 class icon. Issue #187 bumped both up together (14 -> 16 here,
/// 18 -> 20 for `ICON_SIZE`) so the slot's ~0.8x-of-the-icon proportion —
/// smaller, secondary — is preserved rather than just growing one.
pub(crate) const IMAGINE_SIZE: f32 = 16.0;

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Gap between the class icon and Imagine slot 0, and between slot 0 and
/// slot 1. Not sourced from the reference meter — chosen so the gutter
/// arithmetic (`IMAGINE_GUTTER_WIDTH`) lands cleanly.
pub(crate) const IMAGINE_GAP: f32 = 2.0;

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Width the two Imagine slots add to `ICON_GUTTER_WIDTH`: two
/// `(gap + slot)` pairs, `32.0`. A single named addend so D4's takedown is
/// mechanical — deleting this line (and its use below) restores
/// `ICON_GUTTER_WIDTH` to its pre-issue-#33 `25.0` with no other
/// arithmetic to touch.
pub(crate) const IMAGINE_GUTTER_WIDTH: f32 = 2.0 * (IMAGINE_GAP + IMAGINE_SIZE);

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Dim fill for the blank-circle placeholder an empty, unknown-id, or
/// undecoded-texture Imagine slot paints instead of an icon — in the same
/// register as `CLASS_ICON_TINT`.
pub(crate) const IMAGINE_SLOT_EMPTY: egui::Color32 = egui::Color32::from_rgb(0x55, 0x55, 0x55);

/// Highest Imagine tier (issues #169/#170), the gate for the gold ring
/// `draw_row` paints around a filled, maxed-out slot.
///
/// **Inferred, not directly observed.** This repo's only real packet
/// capture (`crates/app/tests/fixtures/dump-2976-boss-fight.jsonl.zst`)
/// happened to carry no nonzero `remodel_level` samples at all — see
/// `attrs::decode_skill_ids`'s doc comment — so neither whether the wire
/// value is 0-based or 1-based, nor whether `5` is truly the ceiling, has
/// been seen in the wild. `5` is taken from BPSR-ZDPS's own domain naming
/// (`Tier`) alone. `decode_skill_ids` logs the first live nonzero
/// `remodel_level` it observes at `debug`, specifically so a real capture's
/// log file can eventually confirm or correct this constant.
///
/// The ring predicate (`imagine_ring_visible`) gates on `tier >=
/// IMAGINE_MAX_TIER` rather than `tier == IMAGINE_MAX_TIER` on purpose: if
/// live tiers turn out to run higher than 5 (this constant is wrong-low),
/// `>=` still fires once the real max is reached instead of never firing;
/// the failure mode of `>=` under a wrong-high guess is merely "no ring
/// yet" until data proves otherwise, never a stuck-wrong ring.
pub(crate) const IMAGINE_MAX_TIER: i32 = 5;

/// Stroke color of the gold/amber ring `draw_row` paints around a
/// filled Imagine slot at `IMAGINE_MAX_TIER` (issue #170). Issue #180:
/// shifted off the original `#FFD700` ("gold" in name only — it rendered
/// as flat yellow) to `#D4AF37`, a warmer amber/gold that sits in the
/// `#D4AF37`-`#C9A227` range and still reads distinct from
/// `CLASS_ICON_TINT`'s neutral light gray as a deliberate highlight, not a
/// tint variation.
pub(crate) const IMAGINE_MAX_TIER_RING_COLOR: egui::Color32 =
    egui::Color32::from_rgb(0xD4, 0xAF, 0x37);

/// Width of the gold max-tier ring's stroke (issue #170). Issue #180:
/// thinned from `1.5` to `1.0` so the ring reads as a thin accent rather
/// than dominating the 16pt `IMAGINE_SIZE` slot it circles.
pub(crate) const IMAGINE_MAX_TIER_RING_WIDTH: f32 = 1.0;

/// Hover-tooltip text for an equipped Imagine slot (issue #169): the plain
/// `name` when `tier` is absent or the wire-default `0` (proto3's
/// omit-when-default means "no tier observed yet" and "tier is genuinely
/// zero" are indistinguishable on the wire, so both read as "nothing to
/// add"), otherwise `"{name} · Tier {tier}"`.
pub(crate) fn imagine_hover_text(name: &str, tier: Option<i32>) -> String {
    match tier {
        Some(t) if t > 0 => format!("{name} · Tier {t}"),
        _ => name.to_string(),
    }
}

/// Whether a filled Imagine slot should get the gold max-tier ring (issue
/// #170): `tier >= IMAGINE_MAX_TIER`. `None` (unresolved/no tier data) and
/// any tier below the max both yield `false` — see `IMAGINE_MAX_TIER`'s doc
/// comment for why this is `>=` rather than `==`.
pub(crate) fn imagine_ring_visible(tier: Option<i32>) -> bool {
    tier.is_some_and(|t| t >= IMAGINE_MAX_TIER)
}

/// Fixed left-hand gutter `draw_row` reserves for the class icon plus its
/// two Imagine slots (issue #33): a margin, the class icon, the Imagine
/// gutter, then a matching margin — reserved whether or not this
/// particular row has any of these to paint, so every row's name still
/// starts at the same x (see `icon_slots`).
pub(crate) const ICON_GUTTER_WIDTH: f32 =
    ICON_MARGIN + ICON_SIZE + IMAGINE_GUTTER_WIDTH + ICON_MARGIN;

/// A row's class-icon slot, its two Imagine slots (issue #33), and the
/// x-offset from the row rect's left edge at which the player name should
/// then start.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct RowIconSlots {
    pub(crate) class: egui::Rect,
    pub(crate) imagines: [egui::Rect; 2],
    pub(crate) name_offset: f32,
}

/// Computes a row's class icon slot (a square, vertically centered in
/// `rect`, inset from the left edge by `ICON_MARGIN`), its two Imagine
/// slots immediately to its right (issue #33), and the x-offset from
/// `rect`'s left edge at which the player name should then start. Pure
/// geometry — it never looks at whether this row actually has a class icon
/// to paint or any equipped Imagines — so the slots, and therefore the
/// name's start position, are identical across every row regardless of
/// which classes have icons or which Imagines are equipped.
pub(crate) fn icon_slots(rect: egui::Rect) -> RowIconSlots {
    let class = egui::Rect::from_min_size(
        egui::pos2(rect.left() + ICON_MARGIN, rect.center().y - ICON_SIZE / 2.0),
        egui::vec2(ICON_SIZE, ICON_SIZE),
    );
    // IMAGINE-TAKEDOWN: one of five sites — see
    // `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
    let imagines = std::array::from_fn(|i| {
        let left = rect.left()
            + ICON_MARGIN
            + ICON_SIZE
            + IMAGINE_GAP
            + i as f32 * (IMAGINE_SIZE + IMAGINE_GAP);
        egui::Rect::from_min_size(
            egui::pos2(left, rect.center().y - IMAGINE_SIZE / 2.0),
            egui::vec2(IMAGINE_SIZE, IMAGINE_SIZE),
        )
    });
    let name_offset = ICON_GUTTER_WIDTH + NAME_LEFT_PAD;
    RowIconSlots {
        class,
        imagines,
        name_offset,
    }
}

/// `bpsr_meter` already fills unknown names with `Player {uid}`; this is a
/// defensive fallback in case a row ever arrives with an empty name.
pub(crate) fn row_name(row: &PlayerRow) -> String {
    if row.name.is_empty() {
        format!("Player {}", row.uid)
    } else {
        row.name.clone()
    }
}

/// Composes the bracketed suffix `draw_row` paints immediately after the
/// player name when `AbilityScore` and/or `SeasonStrength` are enabled
/// (issue #168) — the two columns `Settings::stat_columns` excludes from
/// the ordinary stat-column layout because they read better folded into
/// the name slot than laid out as their own leading columns (the same
/// reasoning `ColumnKind::ALL`'s doc comment already gives for why they
/// lead the canonical column order, and `ColumnKind::renders_inline_with_
/// name`'s doc comment restates for this issue specifically).
///
/// Format, decided by the issue rather than left to this function: a
/// single bracketed group, Ability Score first then Season Strength,
/// joined by `" / "` when both are present (`"[12345 / 678]"`), or just
/// the one enabled value's own brackets when only one is on
/// (`"[12345]"`). Each value comes from that column's own `StatColumn::
/// text` (`ColumnKind::spec`), so the None-is-blank behavior already
/// documented there carries over unchanged: a `None` reading (no
/// FIGHT_POINT packet seen yet for this player) produces an empty string
/// from `text`, which this function treats as "omit this slot" rather
/// than painting an empty bracket entry. `None` is returned — no
/// brackets, no leading space — when neither column is enabled, or both
/// are enabled but both values are still `None`; `draw_row` paints
/// nothing extra after the name in either case.
///
/// This function never truncates, elides, or otherwise clamps the
/// returned string to any width budget, and never will — that is an
/// explicit issue #168 follow-up decision, not an oversight: a long name
/// plus both scores is allowed to run into (and, since `draw_row` paints
/// it before the stat-column loop, visually underneath) the stat columns
/// rather than lose characters. `draw_row`'s own comment at the paint
/// site has the z-order reasoning; this function's job stays just the
/// string, in full, every time.
pub(crate) fn name_suffix(row: &PlayerRow, settings: &Settings) -> Option<String> {
    let mut parts = Vec::new();
    if settings.is_visible(ColumnKind::AbilityScore) {
        let text = (ColumnKind::AbilityScore.spec().text)(row);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    if settings.is_visible(ColumnKind::SeasonStrength) {
        let text = (ColumnKind::SeasonStrength.spec().text)(row);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("[{}]", parts.join(" / ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
    // -- Imagine tier hover text / gold ring (issues #169/#170) -------------

    #[test]
    fn imagine_hover_text_plain_name_when_tier_is_zero() {
        assert_eq!(
            imagine_hover_text("Stunt! Boarrier Rush", Some(0)),
            "Stunt! Boarrier Rush"
        );
    }

    #[test]
    fn imagine_hover_text_plain_name_when_tier_is_unresolved() {
        assert_eq!(
            imagine_hover_text("Stunt! Boarrier Rush", None),
            "Stunt! Boarrier Rush"
        );
    }

    #[test]
    fn imagine_hover_text_appends_tier_when_positive() {
        assert_eq!(
            imagine_hover_text("Stunt! Boarrier Rush", Some(3)),
            "Stunt! Boarrier Rush · Tier 3"
        );
    }

    #[test]
    fn imagine_hover_text_appends_max_tier() {
        assert_eq!(
            imagine_hover_text("Stunt! Boarrier Rush", Some(IMAGINE_MAX_TIER)),
            "Stunt! Boarrier Rush · Tier 5"
        );
    }

    #[test]
    fn imagine_ring_visible_false_when_tier_unresolved() {
        assert!(!imagine_ring_visible(None));
    }

    #[test]
    fn imagine_ring_visible_false_below_max_tier() {
        assert!(!imagine_ring_visible(Some(IMAGINE_MAX_TIER - 1)));
        assert!(!imagine_ring_visible(Some(0)));
    }

    #[test]
    fn imagine_ring_visible_true_at_max_tier() {
        assert!(imagine_ring_visible(Some(IMAGINE_MAX_TIER)));
    }

    /// Gated `>=` rather than `==` (see `IMAGINE_MAX_TIER`'s doc comment):
    /// if live data ever shows a tier above the guessed max, the ring
    /// should still fire rather than silently stop.
    #[test]
    fn imagine_ring_visible_true_above_max_tier() {
        assert!(imagine_ring_visible(Some(IMAGINE_MAX_TIER + 1)));
    }

    /// `demo_snapshot` now carries each row's Imagine slots on the same
    /// `DEMO_ROWS` tuple entry rather than a separate by-index array, so a
    /// row and its Imagines can't drift apart structurally — but pin one
    /// specific row's name to its exact Imagine ids anyway, so an
    /// accidental reorder of `DEMO_ROWS` (which would carry the wrong name
    /// to the wrong ids) still fails a test, not just a compile.
    #[test]
    fn demo_snapshot_pairs_each_row_with_its_own_imagines() {
        let snapshot = demo_snapshot();
        let glorbaxian = snapshot
            .rows
            .iter()
            .find(|row| row.name == "Glorbaxian")
            .expect("demo snapshot must include Glorbaxian");
        assert_eq!(glorbaxian.imagines, [Some(3903), Some(3904)]);
        assert_eq!(glorbaxian.imagine_tiers, [Some(IMAGINE_MAX_TIER), Some(2)]);
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

    #[test]
    fn name_suffix_is_none_when_neither_column_is_enabled() {
        let row = sample_score_row(Some(12345), Some(678));
        assert_eq!(name_suffix(&row, &Settings::default()), None);
    }

    #[test]
    fn name_suffix_shows_ability_score_alone() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        let row = sample_score_row(Some(12345), Some(678));
        assert_eq!(name_suffix(&row, &settings), Some("[12345]".to_string()));
    }

    #[test]
    fn name_suffix_shows_season_strength_alone() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::SeasonStrength);
        let row = sample_score_row(Some(12345), Some(678));
        assert_eq!(name_suffix(&row, &settings), Some("[678]".to_string()));
    }

    #[test]
    fn name_suffix_shows_both_ability_score_then_season_strength() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        let row = sample_score_row(Some(12345), Some(678));
        assert_eq!(
            name_suffix(&row, &settings),
            Some("[12345 / 678]".to_string())
        );
    }

    #[test]
    fn name_suffix_omits_a_none_value_slot_when_both_enabled() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        let row = sample_score_row(None, Some(678));
        assert_eq!(name_suffix(&row, &settings), Some("[678]".to_string()));
    }

    #[test]
    fn name_suffix_is_none_when_both_enabled_but_both_values_are_none() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        let row = sample_score_row(None, None);
        assert_eq!(name_suffix(&row, &settings), None);
    }

    /// Issue #168 follow-up: the suffix is allowed to visually overlap the
    /// stat columns rather than being truncated/elided, so `name_suffix`
    /// itself must never clamp its output to any width or length budget —
    /// widest-plausible-value in (ability score's real in-game ceiling is
    /// 5 digits, season strength's is 4, per `ColumnKind::spec`'s doc
    /// comments) must come back out whole, not cut down to fit anything.
    #[test]
    fn name_suffix_is_never_truncated_for_the_widest_plausible_values() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        let row = sample_score_row(Some(99_999), Some(9_999));
        assert_eq!(
            name_suffix(&row, &settings),
            Some("[99999 / 9999]".to_string())
        );
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
    ///
    /// Issue #118 also moved which of `fmt_short`'s own bands is widest:
    /// its coarsest, 0-decimal band ("1000K") is one digit *shorter* than
    /// its 2-/1-decimal bands ("99.99M"/"999.9M"), so `999_950 -> "1000K"`
    /// — the value this test used pre-#118, back when 0-decimal was
    /// `fmt_dps`'s *only* band — stopped being the widest case the day
    /// #118 shipped, and a version of this test that still used it would
    /// have missed the `Dps` column gap entirely. `widest_row` below now
    /// uses `99_999_000 -> "99.99M"` instead. Character count alone
    /// doesn't settle which band is widest, either: "1000K" and "99.99M"
    /// differ by whole digits, but "99.99M" and "999.9M" are both 6 chars
    /// and only measuring the real galley (as this test does, not counting
    /// characters) shows they render identically wide — and that "M" is
    /// the pixel-widest of the K/M/B suffixes in this font, wider even
    /// than some 7-char strings in a narrower suffix.
    #[test]
    fn widest_formatted_text_fits_its_column_width_budget() {
        let ctx = egui::Context::default();
        // Load the real (non-empty) default fonts, so glyph metrics match
        // what `draw_row` actually paints with, then discard the resulting
        // font-atlas texture upload — nothing is painted in this test.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        // Widest plausible value for every field any column formats:
        // `fmt_short` (issue #118 unified `fmt_dps` into it — there is only
        // one abbreviator now) keeps its scaled digit run to a 5-char
        // pre-suffix budget, but the widest-*rendering* value is not the
        // widest-*digit-count* one — see the doc comment above. Use
        // `99_999_000 -> "99.99M"`, the actual widest case, rather than
        // the 0-decimal `999_950 -> "1000K"` this test used to check.
        // `fmt_share`'s and `fmt_pct0`'s (issue #80.2's 0-decimal variant)
        // worst cases are unaffected by #118. `ability_score`/
        // `season_strength` are the two exceptions: they render the full,
        // un-abbreviated digit string (owner requirement), so their widest
        // plausible input is their real in-game ceiling — ability score is
        // a 5-digit stat (max 99_999) and season strength is a 4-digit
        // stat (max 9_999), per the repo owner — rather than the field
        // type's own ceiling (`u32::MAX`) or a `fmt_short`-derived value.
        // Do not "fix" these back to `u32::MAX`.
        assert_eq!(fmt_short(99_999_000), "99.99M");
        assert_eq!(fmt_share(100.0), "100.0%");
        assert_eq!(fmt_pct0(100.0), "100%");
        let widest_row = PlayerRow {
            uid: 1,
            name: String::new(),
            class: None,
            damage: 99_999_000,
            dps: 99_999_000.0,
            share_pct: 100.0,
            crit_pct: 100.0,
            lucky_pct: 100.0,
            hits: 99_999_000,
            // A death count is a 1-2 digit figure in practice; 99 is the
            // widest plausible one, not `u32::MAX`, same reasoning as the
            // in-game ceilings above.
            deaths: 99,
            // `fmt_duration`'s documented worst case, `120:00` — the pill
            // is not a column, so this only keeps the fixture honest.
            dead_ms: Some(120 * 60 * 1000),
            ability_score: Some(99_999),
            season_strength: Some(9_999),
            imagines: [Some(99_999), Some(99_999)],
            imagine_tiers: [Some(IMAGINE_MAX_TIER), Some(IMAGINE_MAX_TIER)],
            skills: Vec::new(),
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            buffs: Vec::new(),
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

    #[test]
    fn icon_slots_class_is_square() {
        let slots = icon_slots(row_rect());
        assert_eq!(slots.class.width(), ICON_SIZE);
        assert_eq!(slots.class.height(), ICON_SIZE);
    }

    #[test]
    fn icon_slots_class_is_inset_from_the_rows_left_edge_by_the_margin() {
        let rect = row_rect();
        let slots = icon_slots(rect);
        assert_eq!(slots.class.left(), rect.left() + ICON_MARGIN);
    }

    #[test]
    fn icon_slots_class_is_vertically_centered_in_the_row() {
        let rect = row_rect();
        let slots = icon_slots(rect);
        assert_eq!(slots.class.center().y, rect.center().y);
    }

    #[test]
    fn icon_slots_name_offset_clears_the_icon_with_its_own_margin() {
        let rect = row_rect();
        let slots = icon_slots(rect);
        // The name must start at or after the icon's right edge plus its own
        // margin gap — never overlapping the icon.
        assert!(rect.left() + slots.name_offset >= slots.class.right() + ICON_MARGIN);
    }

    #[test]
    fn icon_slots_name_offset_equals_the_gutter_plus_name_pad() {
        let slots = icon_slots(row_rect());
        assert_eq!(slots.name_offset, ICON_GUTTER_WIDTH + NAME_LEFT_PAD);
    }

    /// The slots are reserved unconditionally: their geometry depends only
    /// on the row rect, never on anything row-specific like whether this
    /// player's class actually has an icon or any Imagines equipped — so
    /// identical rects must always yield identical slots.
    #[test]
    fn icon_slots_is_independent_of_row_width() {
        let narrow =
            egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(50.0, ROW_HEIGHT));
        let wide =
            egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(500.0, ROW_HEIGHT));
        assert_eq!(icon_slots(narrow), icon_slots(wide));
    }

    #[test]
    fn icon_slots_imagines_are_square_and_vertically_centered() {
        let rect = row_rect();
        let slots = icon_slots(rect);
        for slot in slots.imagines {
            assert_eq!(slot.width(), IMAGINE_SIZE);
            assert_eq!(slot.height(), IMAGINE_SIZE);
            assert_eq!(slot.center().y, rect.center().y);
        }
    }

    #[test]
    fn icon_slots_imagine_zero_starts_the_gap_right_of_the_class_icon() {
        let slots = icon_slots(row_rect());
        assert_eq!(slots.imagines[0].left(), slots.class.right() + IMAGINE_GAP);
    }

    #[test]
    fn icon_slots_imagine_one_starts_the_gap_right_of_slot_zero() {
        let slots = icon_slots(row_rect());
        assert_eq!(
            slots.imagines[1].left(),
            slots.imagines[0].right() + IMAGINE_GAP
        );
    }

    #[test]
    fn icon_slots_imagines_never_overlap_the_class_icon_or_the_name() {
        let rect = row_rect();
        let slots = icon_slots(rect);
        assert!(slots.imagines[0].left() >= slots.class.right());
        assert!(
            rect.left() + slots.name_offset >= slots.imagines[1].right() + ICON_MARGIN,
            "name must start at or after the last imagine slot's right edge plus ICON_MARGIN"
        );
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

    /// Issue #97: a row doing real damage but far below the top row (e.g. a
    /// support/healer next to a burst DPS class) must still paint a visible
    /// sliver of bar rather than reading as if it did zero damage.
    #[test]
    fn row_bar_frac_floors_a_near_zero_positive_share_to_the_visible_minimum() {
        // 1 out of 1,000,000 rounds to a fraction far below the floor.
        assert_eq!(row_bar_frac(1, 1_000_000), ROW_BAR_MIN_FRAC);
    }

    /// The floor must never kick in for a row that truly did zero damage —
    /// that row stays fully empty, not a fake sliver.
    #[test]
    fn row_bar_frac_stays_zero_for_zero_damage() {
        assert_eq!(row_bar_frac(0, 1_000_000), 0.0);
    }

    /// A share already at or above the floor must pass through unchanged,
    /// not get clamped down to the floor.
    #[test]
    fn row_bar_frac_leaves_a_share_already_above_the_floor_untouched() {
        assert_eq!(row_bar_frac(500_000, 1_000_000), 0.5);
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

    /// Issue #248: `Name` used to be a flat 160.0 guess, narrow enough that
    /// a real, generously-long skill name clipped against the next column
    /// with almost no trailing gap. This renders the widest real name found
    /// (`shinra-skills-ex.webp`'s "Harmonious Fire Avalanche") through the
    /// exact font `draw_skill_window`'s row loop paints `Name` cells with
    /// (`regular(FONT_SIZE_ROW)`) and checks the column's stated width
    /// clears it by at least the reference's own tightest inter-column
    /// header gap (16px, measured between "Avg crit" and "Avg white").
    #[test]
    fn skill_name_column_clears_the_widest_real_skill_name_before_the_next_column() {
        let ctx = egui::Context::default();
        // Load the real (non-empty) default fonts so glyph metrics match
        // what the row loop actually paints with.
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let widest = "Harmonious Fire Avalanche";
        let text_width = ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                widest.to_owned(),
                regular(FONT_SIZE_ROW),
                egui::Color32::WHITE,
            )
            .rect
            .size()
            .x
        });
        let gap = skills::SkillColumn::Name.width() - text_width;
        assert!(
            gap >= 16.0,
            "Name's width must clear \"{widest}\" ({text_width}pt) by at least the \
             reference's tightest column gap (16px), got {gap}"
        );
    }

    /// Every column header label must fit the width its column reserves,
    /// on every tab and in every sort state. None of them were measured
    /// before: the widths were eyeballed off `fmt_short`'s longest *value*
    /// only, and `draw_skill_window`'s header loop paints labels
    /// right-aligned on each column's anchor at fixed size, so a label
    /// wider than its own slot spills *left* over its neighbour. Four of
    /// the six tabs shipped overlapping header rows at both the initial
    /// and the minimum width — Dps and Heal read `Avg criAvg white`, the
    /// two amount tabs read `Amount ↓% Amt`.
    ///
    /// The sort state is half the bug: `SkillSort::header_label` appends
    /// its direction arrow to the active column's label *after* the widths
    /// were chosen, so sorting could push a label out of a slot that fit
    /// it bare (Heal sorted by `% Crit` degraded to `Hea% Hea% Crit ↑`).
    /// So this walks every column of every tab against every sort state
    /// that tab can reach — each of its own columns, both directions —
    /// and measures through the exact font the header loop paints with
    /// (`bold(FONT_SIZE_ROW)`) rather than against any hardcoded
    /// expectation.
    ///
    /// One honest caveat: `bold()` only resolves to the real bold family
    /// when `fonts::has_real_bold()` is true, and that flag stays `false`
    /// here — it is only ever flipped by `install_cjk_fallback`, which this
    /// test never calls (see `fonts.rs`'s own
    /// `no_real_bold_is_reported_before_install_runs`). CI runs on
    /// ubuntu-latest with no `C:\Windows\Fonts`, so this measures egui's
    /// bundled regular-weight fallback, not the real Segoe UI Bold
    /// production paints with — it is a proxy that catches gross budget
    /// errors, not a guarantee the labels fit the real bold glyphs. The
    /// per-label widths that motivate `skills.rs`'s column widths were
    /// measured on the live Windows window, not by this test.
    #[test]
    fn every_tab_header_label_fits_its_column_at_every_sort_state() {
        let ctx = egui::Context::default();
        // Load the real (non-empty) default fonts so glyph metrics match
        // what the header loop actually paints with (see the caveat above:
        // "real" here is still egui's bundled regular weight, since
        // `has_real_bold()` is false in every test run).
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let mut overflowing = Vec::new();
        for tab in skills::SKILL_TABS {
            let sorts: Vec<skills::SkillSort> = tab
                .columns()
                .iter()
                .flat_map(|column| {
                    [true, false].map(|descending| skills::SkillSort {
                        column: *column,
                        descending,
                    })
                })
                .collect();
            for kind in tab.columns() {
                for sort in &sorts {
                    let label = sort.header_label(*kind);
                    let text_width = ctx.fonts_mut(|f| {
                        f.layout_no_wrap(label.clone(), bold(FONT_SIZE_ROW), egui::Color32::WHITE)
                            .rect
                            .size()
                            .x
                    });
                    let complaint = format!(
                        "{tab:?}/{kind:?}: \"{label}\" is {text_width}pt in a {}pt column",
                        kind.width()
                    );
                    // The same label recurs once per sort state that
                    // leaves it bare, so report each distinct overflow
                    // once rather than n times.
                    if text_width > kind.width() && !overflowing.contains(&complaint) {
                        overflowing.push(complaint);
                    }
                }
            }
        }
        assert!(
            overflowing.is_empty(),
            "header labels overflow their columns and overdraw their \
             neighbours: {overflowing:?}"
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
            window_size: None,
            ..Settings::default()
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
    fn default_inner_width_matches_icon_gutter_plus_name_budget_plus_columns() {
        let columns_width: f32 = stat_columns_for(&Settings::default().stat_columns())
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
        // Issue #80.1 tightened the default columns' widths (`Dps`,
        // `CritPct`, `LuckyPct`), shrinking this from its old `451.0` to
        // `387.0`; issue #33's two Imagine slots then widened the icon
        // gutter by `IMAGINE_GUTTER_WIDTH` (`32.0`), landing here at
        // `419.0`. Issue #118's 2-/1-decimal bands then widened
        // `fmt_short`'s true worst case past `Dps`'s old 48.0-wide budget
        // (see that column's comment in `settings.rs`), growing it to
        // 56.0 and landing here at `427.0`. Issue #187 then bumped
        // `ICON_SIZE` (18 -> 20) and `IMAGINE_SIZE` (14 -> 16), widening
        // `ICON_GUTTER_WIDTH` by `6.0` (2 for the icon, 2 * 2 for the two
        // Imagine slots) and landing here at `433.0`.
        assert_eq!(default_inner_width(), 433.0);
    }

    #[test]
    fn default_inner_width_exceeds_the_default_stat_columns_width() {
        let columns_width: f32 = stat_columns_for(&Settings::default().stat_columns())
            .iter()
            .map(|c| c.width)
            .sum();
        assert!(
            default_inner_width() > columns_width,
            "default width must leave room for a name in front of the {}pt of stat columns",
            columns_width
        );
    }

    // -- draw_rows scrolling (issue #84) and the row-pitch/centering
    // regression harness (issue #83) ---------------------------------------

    #[test]
    fn row_content_width_matches_the_viewport_when_columns_need_no_shrinking() {
        let total = 200.0;
        let viewport = total + COLUMN_RIGHT_MARGIN + 50.0;
        assert_eq!(row_content_width(viewport, total), viewport);
    }

    #[test]
    fn row_content_width_still_tracks_a_narrowing_viewport_above_the_floor() {
        let total = 200.0;
        let floor = total * MIN_COLUMN_SCALE + COLUMN_RIGHT_MARGIN;
        let viewport = floor + 10.0;
        assert_eq!(row_content_width(viewport, total), viewport);
    }

    #[test]
    fn row_content_width_pins_to_the_floor_once_the_viewport_drops_below_it() {
        let total = 200.0;
        let floor = total * MIN_COLUMN_SCALE + COLUMN_RIGHT_MARGIN;
        let viewport = floor - 20.0;
        let content = row_content_width(viewport, total);
        assert_eq!(content, floor);
        assert!(
            content > viewport,
            "content {content} must exceed the {viewport}pt viewport, or the ScrollArea has nothing to scroll to"
        );
    }

    #[test]
    fn scrolling_is_needed_once_the_viewport_narrows_past_the_column_scale_floor() {
        let snapshot = rows_test_snapshot(1);
        let stat_columns_total: f32 = stat_columns_for(&Settings::default().ordered_columns())
            .iter()
            .map(|c| c.width)
            .sum();
        let floor = stat_columns_total * MIN_COLUMN_SCALE + COLUMN_RIGHT_MARGIN;
        let narrow_viewport = floor - 20.0;
        let content = rows_content_size(&snapshot, narrow_viewport, ROW_HEIGHT * 2.0);
        assert!(
            content.x > narrow_viewport,
            "content {content:?} must exceed the narrowed {narrow_viewport}pt viewport once columns hit the {MIN_COLUMN_SCALE} floor"
        );
        assert!((content.x - floor).abs() < 0.5);
    }

    /// Issue #83's regression harness: proves `draw_rows`' pitch and
    /// per-row centering directly from what it *paints*, mirroring
    /// `HeaderFrame`/`header_painted_boxes` (issue #75) rather than trusting
    /// that `ROW_HEIGHT`/`allocate_exact_size` never drift from what
    /// actually lands on screen. Renders inside the `ScrollArea` issue #84
    /// adds — `rows_painted_boxes` measures shapes intersected with the
    /// clip rect they actually painted under, whatever that clip came from.
    #[test]
    fn rows_painted_boxes_are_exactly_row_height_apart() {
        let snapshot = rows_test_snapshot(DEFAULT_VISIBLE_ROWS);
        let frame =
            rows_painted_boxes(&snapshot, default_inner_width(), default_inner_height(None));
        let rows = frame.row_rects();
        assert_eq!(
            rows.len(),
            DEFAULT_VISIBLE_ROWS,
            "expected one row rect per player row: {rows:?}"
        );
        for pair in rows.windows(2) {
            let gap = pair[1].top() - pair[0].top();
            assert!(
                (gap - ROW_HEIGHT).abs() < 0.01,
                "consecutive rows {pair:?} are {gap}pt apart, not {ROW_HEIGHT}"
            );
        }
    }

    /// Issue #168's render-level contract, driven through `draw_rows`
    /// rather than through `name_suffix` alone (which the pure-string
    /// tests above already cover): with `AbilityScore`/`SeasonStrength`
    /// enabled, the bracketed suffix must actually reach the screen, sit
    /// flush against the painted name, be dimmed relative to it, and be
    /// painted *before* the stat-column loop so the columns win z-order.
    /// That last point is the whole reason the suffix is allowed to be
    /// unclipped and uncapped, so it is pinned here from paint order, not
    /// assumed from the call site.
    #[test]
    fn the_inline_score_suffix_paints_flush_dimmed_and_beneath_the_stat_columns() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        let row = PlayerRow {
            name: "Zoe".to_string(),
            damage: 1_000,
            dps: 1_234.0,
            ..sample_score_row(Some(12_345), Some(678))
        };
        let snapshot = Snapshot {
            duration_ms: 90_000,
            total_damage: row.damage,
            total_dps: row.dps,
            rows: vec![row.clone()],
            encounter: EncounterInfo::default(),
            capture_alive: true,
        };

        let frame = rows_painted_boxes_with(
            &snapshot,
            &settings,
            default_inner_width(),
            default_inner_height(None),
        );

        // Painted whole, exactly as `name_suffix` composed it, with the
        // single separating space `draw_row` prepends — no elision.
        let suffix_text = format!(" {}", name_suffix(&row, &settings).expect("a suffix"));
        assert_eq!(suffix_text, " [12345 / 678]");
        let name_box = frame.text_box(&row.name);
        let suffix_box = frame.text_box(&suffix_text);
        assert!(
            (suffix_box.left() - name_box.right()).abs() < 0.5,
            "suffix {suffix_box:?} must start where the name {name_box:?} ends"
        );
        assert!(
            (suffix_box.center().y - name_box.center().y).abs() < 0.5,
            "suffix {suffix_box:?} must share the name's baseline row {name_box:?}"
        );

        // Dimmed: the same white as the name, at `NAME_SUFFIX_ALPHA`.
        let (suffix_index, suffix_color) = frame.text_paint(&suffix_text);
        let (name_index, name_color) = frame.text_paint(&row.name);
        assert_eq!(
            suffix_color,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, NAME_SUFFIX_ALPHA)
        );
        assert!(
            suffix_color.a() < name_color.a(),
            "suffix {suffix_color:?} must be dimmer than the name {name_color:?}"
        );

        // Paint order: name, then suffix, then every stat column — so the
        // columns land on top of whatever the suffix bled under them.
        let stat_text = (ColumnKind::Dps.spec().text)(&row);
        let (stat_index, _) = frame.text_paint(&stat_text);
        assert!(
            name_index < suffix_index && suffix_index < stat_index,
            "expected name ({name_index}) then suffix ({suffix_index}) then stat column {stat_text:?} ({stat_index})"
        );

        // And the inline pair really did leave the grid: neither value is
        // painted a second time as its own column.
        assert!(
            !frame.texts.iter().any(|(painted, ..)| painted == "12345"),
            "ability score must not also paint as a stat column: {:?}",
            frame.texts
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
}
