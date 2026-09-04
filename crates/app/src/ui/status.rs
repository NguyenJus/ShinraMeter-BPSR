//! Stat pills and the stat colors shared by the header and the rows.

use super::*;

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
pub(crate) const PILL_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(9, 9, 9, 9);

/// Fill of the timer pill — the source's `#1aaaaaaa`, a light gray at very
/// low alpha. Deliberately not `PILL_FILL`: the duration is the stat row's
/// lead readout and its capsule sits a shade lighter than the two value
/// pills beside it, which is what lets the eye separate "how long" from
/// "how much" without a divider between them.
pub(crate) const TIMER_PILL_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xAA, 0xAA, 0xAA, 0x11);

/// Value text color inside a header/DPS/damage pill — the source's `#afff`:
/// white at ~2/3 alpha, the dimmer, partially transparent sibling of
/// `TITLE_TEXT_COLOR`'s opaque white. The two are deliberately not the same
/// value and must not be collapsed into one: the source keeps the encounter
/// title as the header's visually heaviest element and steps the stat values
/// down behind it, so painting the pills in full white would flatten that
/// hierarchy.
pub(crate) const PILL_VALUE_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0xAA);

/// Glyph tint inside a header/DPS/damage pill — the source's `#5bdf`, a
/// light steel blue distinct from `TOOLBAR_ICON_TINT`'s grayer slate: the
/// stat icons read as an accent, the window controls as chrome.
pub(crate) const PILL_ICON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xBB, 0xDD, 0xFF, 0x55);

/// Side of a header/DPS/damage pill's glyph box, in points — the source's
/// `GeneralStatPathStyle` `14x14`.
pub(crate) const PILL_GLYPH_SIDE: f32 = 14.0;

/// Counter (death) pill fill — `MetricBorderStyle`'s `#1fff`.
pub(crate) const COUNTER_PILL_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x11);

/// Counter (death) pill glyph tint — `MetricPathStyle`'s `#5fff`. Dimmer,
/// via alpha rather than a darker gray, than the (now white)
/// `DEATH_COUNT_RGB` digits beside it.
pub(crate) const COUNTER_ICON_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x55);

/// Side of a counter pill's glyph box, in points — the source's
/// `MetricPathStyle` `12x12`.
pub(crate) const COUNTER_GLYPH_SIDE: f32 = 12.0;

/// Horizontal padding inside a pill, on both ends. Generous on purpose —
/// it is most of what makes the oval read as a container rather than as a
/// highlight behind the text.
pub(crate) const PILL_PAD_X: f32 = 8.0;

/// Vertical padding above and below a pill's text. Small: the pill's height
/// is capped at the button row's height (`pill_size`) so the header band
/// budget (`header_band_height`) stays correct, and the text is what should
/// consume that budget.
pub(crate) const PILL_PAD_Y: f32 = 2.0;

/// Gap between a pill's value text and its icon.
pub(crate) const PILL_ICON_GAP: f32 = 5.0;

/// One stat pill's content. A struct rather than a long argument list
/// because issue #49's death counter and issue #59's timer readout need
/// the same layout with different sizes, colors, glyph sides and corner
/// radii — a positional call would be unreadable at every call site.
pub(crate) struct StatPill<'a> {
    pub(crate) value: &'a str,
    /// The glyph texture, or `None` when its PNG failed to decode (never
    /// expected — the bytes are compile-time constants). `None` paints an
    /// empty icon box so the pill keeps the same width either way, exactly
    /// like `draw_row` reserves a class-icon slot for a class with no icon.
    pub(crate) icon: Option<egui::TextureId>,
    /// Side of the icon's square box, in points. Explicit rather than
    /// derived from the text's line height: the source fixes these per call
    /// site (`GeneralStatPathStyle` 14x14, `MetricPathStyle` 12x12).
    pub(crate) icon_side: f32,
    /// Point size of `value`.
    pub(crate) size: f32,
    pub(crate) value_color: egui::Color32,
    pub(crate) icon_color: egui::Color32,
    /// Icon before the value instead of after it. Every header pill —
    /// timer, DPS and damage alike — reads value-then-icon, matching the
    /// reference render's `02:39 ⏱ | 188.0M/s ☁ | 30.10B ♡`; only issue #49's
    /// per-row death counter reads icon-then-value (skull, then count).
    pub(crate) icon_first: bool,
    /// Per-corner radius. Every pill is a full oval — all four corners at
    /// half the button row's height, never a flattened pair. The timer used
    /// to be a half-pill (`CornerRadius="0 13 13 0"`, welded to the panel's
    /// left border), which is the shape issue #91 fixed.
    pub(crate) corner_radius: egui::CornerRadius,
    /// Fill behind the pill. The timer's (`TIMER_PILL_FILL`) is a shade
    /// lighter than the value pills' `PILL_FILL`.
    pub(crate) fill: egui::Color32,
    /// Optional 1pt outline. No pill has one: the header's timer, DPS and
    /// damage ovals and the per-row counter are all fill-only. The timer
    /// carried the source's hairline `#2fff` border until issue #91 — ringed
    /// among three otherwise bare capsules, it read as an outlined odd one
    /// out rather than as the row's lead readout, and its lighter
    /// `TIMER_PILL_FILL` carries that distinction alone now. Kept as a field
    /// because the chrome is per-call-site and a stroked pill elsewhere
    /// stays a one-line change.
    pub(crate) stroke: Option<egui::Stroke>,
}

impl<'a> StatPill<'a> {
    /// A header stat pill (DPS or total damage): bold value in a light
    /// white, accent icon trailing it — the two ovals right of the timer in
    /// the reference's stat row.
    pub(crate) fn header(value: &'a str, icon: Option<egui::TextureId>) -> Self {
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

    /// The encounter duration: the source's largest stat text, trailed by
    /// its clock glyph, in a capsule of its own — a slightly lighter fill
    /// (`TIMER_PILL_FILL`) than the two value pills, which is the whole of
    /// what marks it as the row's lead readout. The source also ringed it in
    /// a hairline `#2fff`; issue #91 drops that, because one outlined oval
    /// among three bare ones reads as a stray border, not as emphasis.
    ///
    /// Issue #91: the clock used to *lead* the duration. The reference
    /// render reads `02:39 ⏱`, the same value-then-icon order as the DPS
    /// and damage pills beside it.
    ///
    /// Issue #91's real quarrel was never with the capsule but with its
    /// *shape and place*: it was a half-pill (`CornerRadius="0 13 13 0"`)
    /// sitting flush against the panel's left border, so it read as an oval
    /// sliced off by the window edge. The radius is now uniform on all four
    /// corners — a full oval, like `header` — and
    /// `HEADER_STAT_ROW_INSET_X` holds the whole row clear of the border so
    /// nothing crops it. Those two together are the fix; the chrome itself
    /// is the source's and stays.
    pub(crate) fn timer(value: &'a str, icon: Option<egui::TextureId>) -> Self {
        Self {
            value,
            icon,
            icon_side: PILL_GLYPH_SIDE,
            size: FONT_SIZE_TIMER,
            value_color: PILL_VALUE_COLOR,
            icon_color: PILL_ICON_COLOR,
            icon_first: false,
            corner_radius: egui::CornerRadius::same((BUTTON_ROW_HEIGHT / 2.0) as u8),
            fill: TIMER_PILL_FILL,
            stroke: None,
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
    pub(crate) fn counter(
        value: &'a str,
        icon: Option<egui::TextureId>,
        value_color: egui::Color32,
    ) -> Self {
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
pub(crate) fn pill_size(text_size: egui::Vec2, icon_side: f32, max_height: f32) -> egui::Vec2 {
    let width = 2.0 * PILL_PAD_X + text_size.x + PILL_ICON_GAP + icon_side;
    let height = (text_size.y + 2.0 * PILL_PAD_Y).min(max_height);
    egui::vec2(width, height)
}

/// Where a pill's two pieces go inside its rect: the value text's
/// `Align2::LEFT_CENTER` anchor, and the icon's (square, vertically
/// centered) box. Pure geometry so both orderings are unit-testable without
/// a live `egui::Ui` — same reasoning as `icon_slots`.
pub(crate) fn pill_content_layout(
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
pub(crate) fn stat_pill(ui: &mut egui::Ui, pill: StatPill<'_>) -> egui::Response {
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
pub(crate) fn pill_text_size(painter: &egui::Painter, pill: &StatPill<'_>) -> egui::Vec2 {
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
pub(crate) fn paint_stat_pill(
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
pub(crate) const UV_FULL: egui::Rect =
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
    // -- initial_snapshot / drain_snapshots (issue #91 demo mode) -----------

    /// `OverlayApp::new` seeds a real-looking encounter from `demo_snapshot`
    /// when demo mode is on, not the ordinary empty "No target" state — the
    /// whole point of `SHINRA_DEMO` is a populated header to capture.
    #[test]
    fn initial_snapshot_seeds_the_demo_encounter_when_demo_mode_is_on() {
        let snapshot = initial_snapshot(true);
        assert!(!snapshot.rows.is_empty(), "demo mode must seed player rows");
        assert_eq!(
            snapshot.encounter.boss_name,
            Some("Paradox-Calamity Remnant - Final")
        );
    }

    /// Issue #148: the demo header used to show a `total_damage`/`total_dps`
    /// borrowed from the old reference screenshot, independent of
    /// `DEMO_ROWS` — so the header and the rows disagreed by two orders of
    /// magnitude. This guards that the header is always derived from the
    /// rows (not a separate literal), that every row's `share_pct` derives
    /// from that same total, that crit/lucky stay in the plausible 5-70%
    /// band, and that the party has exactly one Tank and one Healer with the
    /// rest on Damage — a realistic dungeon/raid comp, not five DPS.
    #[test]
    fn demo_snapshot_header_and_rows_are_internally_consistent() {
        let snapshot = demo_snapshot();

        let row_damage_sum: i64 = snapshot.rows.iter().map(|row| row.damage).sum();
        assert_eq!(
            snapshot.total_damage, row_damage_sum,
            "header total_damage must equal the sum of the row damages"
        );

        let expected_dps = row_damage_sum as f64 / (snapshot.duration_ms as f64 / 1000.0);
        assert!(
            (snapshot.total_dps - expected_dps).abs() < 0.01,
            "header total_dps must equal total_damage / duration, got {} expected {}",
            snapshot.total_dps,
            expected_dps
        );

        let mut share_sum = 0.0f32;
        for row in &snapshot.rows {
            let expected_share = row.damage as f32 / row_damage_sum as f32 * 100.0;
            assert!(
                (row.share_pct - expected_share).abs() < 0.01,
                "row {}'s share_pct must derive from the row damage sum",
                row.name
            );
            share_sum += row.share_pct;

            assert!(
                (5.0..=70.0).contains(&row.crit_pct),
                "row {}'s crit_pct {} must be within 5-70%",
                row.name,
                row.crit_pct
            );
            assert!(
                (5.0..=70.0).contains(&row.lucky_pct),
                "row {}'s lucky_pct {} must be within 5-70%",
                row.name,
                row.lucky_pct
            );
        }
        assert!(
            (share_sum - 100.0).abs() < 0.1,
            "row shares must sum to ~100%, got {share_sum}"
        );

        let tanks = snapshot
            .rows
            .iter()
            .filter(|row| row.class.and_then(|c| c.role()) == Some(Role::Tank))
            .count();
        let healers = snapshot
            .rows
            .iter()
            .filter(|row| row.class.and_then(|c| c.role()) == Some(Role::Healer))
            .count();
        let damage_dealers = snapshot
            .rows
            .iter()
            .filter(|row| row.class.and_then(|c| c.role()) == Some(Role::Damage))
            .count();
        assert_eq!(tanks, 1, "the demo party must have exactly one tank");
        assert_eq!(healers, 1, "the demo party must have exactly one healer");
        assert_eq!(
            damage_dealers,
            snapshot.rows.len() - 2,
            "everyone else in the demo party must be on the Damage role"
        );
    }

    /// A demo skill breakdown whose damage doesn't sum to the row's own
    /// `damage` would contradict the row it was opened from — precisely the
    /// class of header/row disagreement issue #148 already burned this file
    /// on once for the top-level totals.
    #[test]
    fn demo_skill_damage_sums_to_the_row_damage() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            let skill_damage_sum: i64 = row.skills.iter().map(|s| s.damage).sum();
            assert_eq!(
                skill_damage_sum, row.damage,
                "row {}'s skill damages must sum to its own damage",
                row.name
            );
        }
    }

    /// Issue #75: the timer's clock glyph must not overlap the duration it
    /// sits beside, measured on the shapes `draw_header` *actually paints*,
    /// and the two must share a centerline.
    ///
    /// Issue #91 flipped the ordering: the clock now *trails* the duration
    /// (`02:39 ⏱`), matching the DPS and damage pills and the reference
    /// render, so the gap is measured the other way round. Containment
    /// inside the capsule is
    /// `the_header_pills_lay_glyph_and_value_out_inside_their_box`'s job;
    /// what this test owns is that the two marks neither overlap nor drift
    /// off a shared centerline in the frame actually painted.
    #[test]
    fn the_timer_pills_clock_glyph_never_overlaps_its_value() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);

        let value = frame.text_box(&fmt_duration(snapshot.duration_ms));
        let glyphs = frame.glyph_boxes(GlyphIcon::Timer);
        assert_eq!(
            glyphs.len(),
            1,
            "expected exactly one clock blit: {glyphs:?}"
        );
        let glyph = glyphs[0];

        assert!(
            glyph.left() >= value.right(),
            "the clock glyph {glyph:?} is not to the right of the duration {value:?} — \
             the header reads value-then-icon"
        );
        let gap = glyph.left() - value.right();
        assert!(
            gap >= PILL_ICON_GAP - 0.01,
            "the duration {value:?} and the clock glyph {glyph:?} are only {gap}pt apart \
             (want {PILL_ICON_GAP})"
        );
        let drift = (glyph.center().y - value.center().y).abs();
        assert!(
            drift <= 1.0,
            "the clock glyph {glyph:?} and the value {value:?} are {drift}pt off \
             a shared centerline"
        );
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
    /// ordering issue #49's skull-then-count counter (and the timer readout)
    /// need.
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
            ("dps", StatPill::header("99.99M/s", None)),
            ("damage", StatPill::header("30.10B", None)),
        ] {
            let text = ctx.fonts_mut(|f| {
                f.layout_no_wrap(pill.value.to_owned(), bold(pill.size), pill.value_color)
                    .rect
                    .size()
            });
            // Issue #91: all three header readouts are value-then-icon
            // (`02:39 ⏱ | 188.0M/s ☁ | 30.10B ♡`) — the timer's leading clock
            // was ours, not the reference's.
            assert!(
                !pill.icon_first,
                "{name}: the header reads value-then-icon, not icon-first"
            );
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
            assert!(
                icon_rect.left() >= text_rect.right(),
                "{name}: the glyph {icon_rect:?} is not to the right of the value \
                 {text_rect:?}"
            );
            let gap = icon_rect.left() - text_rect.right();
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

    /// Every header pill is a full oval: a corner radius of at least half
    /// the button row height, *equal on all four corners*. The four-way
    /// equality is the regression guard for issue #91's actual bug — the
    /// timer's `CornerRadius="0 13 13 0"`, whose zeroed west corners made
    /// it a half-pill welded to the window border. Its capsule is not the
    /// problem and is deliberately kept (see `StatPill::timer`); a
    /// west-flattened radius on any pill is.
    #[test]
    fn every_header_pill_is_a_full_oval_and_none_is_a_half_pill() {
        let timer = StatPill::timer("1", None);
        let header = StatPill::header("1", None);

        for (name, pill) in [("header", &header), ("timer", &timer)] {
            let r = pill.corner_radius;
            assert!(
                r.nw == r.sw && r.nw == r.ne && r.nw == r.se,
                "{name}: corner radii nw={} sw={} ne={} se={} are not all equal — \
                 a flattened pair is the half-pill bug",
                r.nw,
                r.sw,
                r.ne,
                r.se
            );
            assert!(r.nw > 0, "{name}: radius {} is square, not an oval", r.nw);
            assert!(
                r.nw as f32 >= BUTTON_ROW_HEIGHT / 2.0 - 1.0,
                "{name}: radius {} is not a full pill",
                r.nw
            );
        }

        // Both pills wear chrome, and the timer's is its own — but chrome
        // here means *fill*. The timer's capsule is a shade lighter than the
        // value pills' and that alone marks it as the lead readout; issue
        // #91 dropped the source's hairline border, which drew a ring round
        // one of three otherwise bare ovals. No pill is stroked.
        assert_ne!(header.fill, egui::Color32::TRANSPARENT);
        assert_ne!(timer.fill, egui::Color32::TRANSPARENT);
        assert_eq!(timer.fill, TIMER_PILL_FILL);
        assert_ne!(timer.fill, header.fill);
        for (name, pill) in [("header", &header), ("timer", &timer)] {
            assert!(
                pill.stroke.is_none(),
                "{name}: {:?} rings the oval — no header pill is stroked",
                pill.stroke
            );
        }
    }

    /// Clicking the Reset button, now that it lives in the toggle cluster
    /// instead of the header dropdown (issue #82), sends the same
    /// `UiCommand::Reset` the old dropdown item did.
    #[test]
    fn clicking_reset_sends_the_reset_command() {
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
        let reset_pos = accessible_rect_for_label(&update, "Reset").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(reset_pos), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let commands = output
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
            !commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::Screenshot(_))),
            "Reset must not also request a screenshot: {commands:?}"
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

    /// Issue #183: pinning the overlay locks its position as well as its
    /// Z-order — the drag band refuses to start a move — and an *in-flight*
    /// move is cancelled, since the pin button lives in the same header the
    /// pointer is already dragging. A resize in flight is left alone.
    #[test]
    fn pinning_blocks_and_cancels_a_move_but_never_a_resize() {
        assert!(drag_locked_by_pin(true));
        assert!(!drag_locked_by_pin(false));

        let window_rect =
            || egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));

        let mut gesture = WindowGesture::default();
        gesture.begin(GestureKind::Move, egui::pos2(10.0, 10.0), window_rect());
        cancel_move_gesture_when_pinned(&mut gesture, false);
        assert_eq!(
            gesture.kind(),
            Some(GestureKind::Move),
            "an unpinned overlay must keep dragging"
        );
        cancel_move_gesture_when_pinned(&mut gesture, true);
        assert_eq!(gesture.kind(), None, "pinning must end the move");

        let resize = GestureKind::Resize(egui::ResizeDirection::West);
        gesture.begin(resize, egui::pos2(10.0, 10.0), window_rect());
        cancel_move_gesture_when_pinned(&mut gesture, true);
        assert_eq!(
            gesture.kind(),
            Some(resize),
            "pinning is about position, not size"
        );
    }

    /// Issue #231: the header dropdown's Columns list can grow taller than
    /// the screen it opens on, so the scroll area wrapping the menu body is
    /// capped at the available screen height (minus a fixed margin) rather
    /// than left unbounded.
    #[test]
    fn header_menu_scroll_max_height_reserves_a_margin_off_the_full_screen() {
        assert_eq!(header_menu_scroll_max_height(800.0, 24.0), 776.0);
    }

    /// Issue #118: `fmt_short` keeps a K/M/B-scaled value's digit run
    /// ~4 significant figures wide by picking the decimal count from the
    /// *scaled* value's own magnitude — 2 decimals below 100, 1 below
    /// 1000, 0 at or above 1000 — matching the row values (`55.30M` ..
    /// `10.30M`) from the reference render exactly; `188_000_000` sits in
    /// the 1-decimal band (`188.0M`), unlike the old `fmt_dps`'s
    /// zero-decimal-at-3-digits rule that used to match the header pill's
    /// `188M/s` exactly — issue #118 traded that pixel match for a
    /// consistent significant-figure rule across every magnitude.
    /// `100_000` pins the 2-decimal/1-decimal band boundary itself (the
    /// scaled value hits exactly `100`, so `100.0K`, not `100.00K`) —
    /// nothing else here lands exactly on that threshold.
    #[test]
    fn fmt_short_table() {
        let cases: [(i64, &str); 9] = [
            (999, "999"),
            (1_000, "1.00K"),
            (100_000, "100.0K"),
            (10_300_000, "10.30M"),
            (17_800_000, "17.80M"),
            (55_300_000, "55.30M"),
            (188_000_000, "188.0M"),
            (999_950, "1000K"),
            (-55_300_000, "-55.30M"),
        ];
        for (input, expected) in cases {
            assert_eq!(fmt_short(input), expected, "fmt_short({input})");
        }
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

    // -- dungeon final-boss precedence (issue #125) -------------------------

    #[test]
    fn title_prefers_scene_boss_name_over_a_non_boss_mid_dungeon_mech() {
        // The exact issue #125 case: `recompute_boss` selected a mid-dungeon
        // mech (e.g. template 1342, "Boss - Battle Mech 03") that
        // `MonsterType` does not mark a boss, so `is_boss` is false and
        // `boss_name` is `None` — but the dungeon's remembered final boss is
        // known, so the header must show that name, not go blank.
        let e = EncounterInfo {
            boss_monster_id: Some(1342),
            boss_name: None,
            is_boss: false,
            scene_boss_name: Some("Blazing Mech 05"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Blazing Mech 05");
    }

    #[test]
    fn title_names_the_live_raid_boss_currently_being_fought_not_the_remembered_one() {
        // The repo owner's issue #131 raid case: the same scene remembers
        // boss C (whichever of the raid's several final bosses was engaged
        // last, in an earlier pull or an earlier session), but boss A is
        // the one actually being fought right now. The header must track
        // the fight, not the stale remembered name.
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Boss A"),
            is_boss: true,
            scene_boss_name: Some("Boss C"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Boss A");
    }

    #[test]
    fn title_names_the_engaged_boss_in_a_multi_boss_scene_rather_than_the_placeholder() {
        // The placeholder is only for "nothing engaged": once the party
        // pulls, the header names the boss that was selected.
        let e = EncounterInfo {
            boss_monster_id: Some(103_309),
            boss_name: Some("Paradox-Calamity Remnant - Final"),
            is_boss: true,
            multi_boss_scene: true,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Paradox-Calamity Remnant - Final");
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

    /// The backdrop fills the row strip it is given — it is the rows'
    /// background, so anything less would leave bare panel fill showing
    /// beside the artwork.
    #[test]
    fn row_backdrop_covers_the_whole_row_area_it_is_given() {
        let panel = wash_test_panel();
        let available = egui::Rect::from_min_max(egui::pos2(panel.left() + 20.0, 120.0), panel.max);

        let backdrop = row_backdrop_rect(available, panel);

        assert_eq!(
            backdrop.top(),
            available.top(),
            "the backdrop must start where the rows do, under the header"
        );
        assert_eq!(backdrop.left(), available.left());
    }

    // -- default window size (issue #26) ----------------------------------

    #[test]
    fn default_inner_height_fits_twenty_rows_without_scrolling() {
        // The row-content budget alone (20 rows * 30pt) must fit inside the
        // computed default height, with room left over for the header band
        // and separator on top of it.
        let rows_only = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        // Measured against what is actually stacked *above* the roster —
        // the whole header band, the separator and the gap between them —
        // not merely against the rows in isolation, which the 18pt-short
        // window issue #91 inherited would also have passed.
        let chrome = header_band_height(BUTTON_ROW_HEIGHT) + SEPARATOR_HEIGHT + ITEM_SPACING_Y;
        assert!(
            default_inner_height() - chrome >= rows_only,
            "default height {} leaves only {}pt under the {chrome}pt of header \
             chrome — short of the {rows_only}pt the {DEFAULT_VISIBLE_ROWS} rows need",
            default_inner_height(),
            default_inner_height() - chrome
        );
    }

    // -- header menu "Reset to defaults" sizing (issue #203) ---------------

    #[test]
    fn reset_to_defaults_inner_height_fits_five_rows_without_scrolling() {
        // Same shape as `default_inner_height_fits_twenty_rows_without_
        // scrolling`, but for the smaller 5-row sample the header menu's
        // "Reset to defaults" item resizes to rather than a full raid.
        let rows_only = RESET_TO_DEFAULTS_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        let chrome = header_band_height(BUTTON_ROW_HEIGHT) + SEPARATOR_HEIGHT + ITEM_SPACING_Y;
        assert!(
            reset_to_defaults_inner_height() - chrome >= rows_only,
            "reset height {} leaves only {}pt under the {chrome}pt of header \
             chrome — short of the {rows_only}pt the {RESET_TO_DEFAULTS_VISIBLE_ROWS} rows need",
            reset_to_defaults_inner_height(),
            reset_to_defaults_inner_height() - chrome
        );
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
}
