//! The per-player skill breakdown window.

use super::*;

// -- per-player skill breakdown window (issue #16) -----------------------
//
// A second, per-player child viewport (D2), painted with the reference's
// own chrome (D14) rather than the main overlay's `PANEL_FILL`/
// `PANEL_BORDER_COLOR` — `Skills.xaml`'s `#111117`/`#212127` is a
// deliberately distinct window from `TopmostBorderStyle`'s panel, not a
// variant of it.

/// Window/bar fill — the reference's `#111117`. Opaque at rest, the same
/// baseline `PANEL_FILL` now uses (issue #184): both windows put their whole
/// transparency in `settings.opacity`, so a given slider value reads the same
/// on each. Only the *colors* stay distinct (see the block comment above) —
/// #184 is about the opacity response, not about unifying the palettes.
pub(crate) const SKILL_CHROME_FILL: egui::Color32 = egui::Color32::from_rgb(0x11, 0x11, 0x17);
/// Panel/tab background, and the Deaths pill's fill — the reference's
/// `#212127`. Same opaque-baseline rule as `SKILL_CHROME_FILL`.
pub(crate) const SKILL_PANEL_FILL: egui::Color32 = egui::Color32::from_rgb(0x21, 0x21, 0x27);
/// Dps column-header text — the reference's `#ef5350`.
pub(crate) const SKILL_HEADER_RGB: egui::Color32 = egui::Color32::from_rgb(0xef, 0x53, 0x50);
/// An unselected tab's label (issue #245). The reference's tab strip
/// leaves unselected headers on the header band with dimmer text; this is
/// the same read at egui's flat alpha — bright enough to be obviously
/// clickable, clearly behind the selected tab's pure white.
pub(crate) const SKILL_TAB_IDLE_RGB: egui::Color32 = egui::Color32::from_rgb(0x9a, 0x9a, 0xa4);
/// A tab this build cannot fill (issue #245: Buff). Dimmer
/// again than `SKILL_TAB_IDLE_RGB`, so the strip reads honestly at a
/// glance, but still selectable — the body explains the gap.
pub(crate) const SKILL_TAB_UNTRACKED_RGB: egui::Color32 = egui::Color32::from_rgb(0x5e, 0x5e, 0x68);
/// Close glyph — the reference's `LightRed #ff5555`.
pub(crate) const SKILL_CLOSE_RGB: egui::Color32 = egui::Color32::from_rgb(0xff, 0x55, 0x55);
/// Translucent-white row hover — the reference's `#10FFFFFF`. Its alpha is
/// the highlight's own weight; issue #184 multiplies `settings.opacity` in on
/// top of it at the paint site, because this fill is part of the window's
/// chrome layer (it paints straight onto `SKILL_CHROME_FILL`, under the row's
/// text) and would otherwise be left hovering over nothing once the rest of
/// the window faded. The main row list's `row_hover_quads` gradient is
/// knowingly *not* treated this way: it belongs to the row-content layer that
/// #166 keeps at full alpha.
pub(crate) const SKILL_ROW_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x10);

/// Column-header band fill (issue #200). The reference paints three
/// distinct levels, not two: measured at x=860, where the game background
/// behind the window runs continuously across both band edges, the header
/// and tab strip sit at (29,28,33), the row list at (45,44,49) and the
/// column-header band at (51,50,55). Backing the known
/// `SKILL_PANEL_FILL`/`SKILL_CHROME_FILL` pair out of that composite gives
/// the window's alpha as ~0.90, which puts this band a further ~0x09 above
/// `SKILL_PANEL_FILL`. Kept on the same opaque baseline as the other two so
/// `settings.opacity` still reads identically across all of them.
pub(crate) const SKILL_COLUMN_HEADER_FILL: egui::Color32 =
    egui::Color32::from_rgb(0x2a, 0x2a, 0x30);

/// Band heights, all measured off `docs/reference/shinra-skills-ex.webp`
/// (issue #200). That capture is 1:1 with WPF DIPs — its header pill is
/// exactly 34px tall against `Skills.xaml`'s `CornerRadius="17"`, which
/// pins the scale — so the pixel figures port straight to egui points.
/// Header: content top y=2 to y=70. Tab strip: y=71..92. Column header:
/// y=93..132. Rows: a 44px pitch from y=133 (ten row text centers, 156
/// through 553, evenly spaced 44.1px apart).
pub(crate) const SKILL_HEADER_HEIGHT: f32 = 70.0;
pub(crate) const SKILL_TAB_HEIGHT: f32 = 22.0;
pub(crate) const SKILL_COLUMN_HEADER_HEIGHT: f32 = 40.0;
/// The reference's measured 44px row pitch (issue #200) — taller than the
/// main row list's `ROW_HEIGHT` (30.0), so this is its own constant rather
/// than reusing that one. D5/D14 read `Skills.xaml`'s `MinHeight` of 40 as
/// the row height; the rendered capture shows 44 once the row's own padding
/// is included, and the pitch is what the eye actually reads.
pub(crate) const SKILL_ROW_HEIGHT: f32 = 44.0;
/// The per-skill row icon (issue #192), measured off the reference (issue
/// #200): row 1's disc spans x 32..69 and y 136..173, i.e. 38px across in a
/// 44px row — the icon dominates its row rather than sitting as a small
/// bullet beside the name, which 24.0 made it. The vendored PNGs are 48px
/// (`scripts/prep-skill-icons.py`), so this is still a downscale at 100%
/// display scaling.
pub(crate) const SKILL_ICON_SIZE: f32 = 38.0;
/// Per-side inset the issue #275 monogram placeholder disc (and the
/// `SKILL_ICON_EMPTY` fallback beside it) is drawn at, versus the full
/// `SKILL_ICON_SIZE / 2.0` radius vendored skill-icon PNGs occupy in code.
/// Issue #281's live-window pass found the placeholder disc painted at the
/// full 38x38 slot while real vendored art's own baked-in transparent
/// padding gives it a visible footprint of only ~26x28 within that same
/// slot — in a mixed row list, the placeholder read noticeably heavier
/// than its neighbors. `2.5` isn't a reproduction of that rectangle (a
/// circle can't match a rectangle's two different margins); it is a small
/// uniform inset that closes the weight gap without shrinking the disc so
/// far it stops looking like it belongs in the same size class as the
/// other 38px slot content.
pub(crate) const SKILL_ICON_PLACEHOLDER_INSET: f32 = 2.5;
/// Radius the issue #275 monogram placeholder disc and the
/// `SKILL_ICON_EMPTY` fallback are painted at — see
/// `SKILL_ICON_PLACEHOLDER_INSET`.
pub(crate) const SKILL_ICON_PLACEHOLDER_RADIUS: f32 =
    SKILL_ICON_SIZE / 2.0 - SKILL_ICON_PLACEHOLDER_INSET;
/// Fill for a row whose skill has no icon to paint *and* no name to derive
/// a monogram placeholder from (issue #275 — see
/// `paint_skill_icon_placeholder`). Deliberately the same flat disc the
/// Imagine slots degrade to, so an empty slot reads as a deliberate blank
/// rather than a rendering failure. In practice every observed skill id
/// resolves to at least the `Skill #<id>` fallback name
/// (`skills::skill_display_name`), so this now only fires for a
/// hypothetically blank/punctuation-only name — kept rather than removed,
/// since "no derivable glyph" is still a real (if unobserved) case.
pub(crate) const SKILL_ICON_EMPTY: egui::Color32 = egui::Color32::from_rgb(0x33, 0x33, 0x3B);
/// Font size of the issue #275 monogram placeholder's 1-2 character glyph,
/// centered on the `SKILL_ICON_PLACEHOLDER_RADIUS` disc (33pt across, since
/// issue #281 inset it off the full `SKILL_ICON_SIZE` slot) — large enough
/// to read at a glance as a letterform rather than a texture artifact,
/// small enough that two characters ("LS", "FF") stay clear of the disc's
/// edge.
pub(crate) const SKILL_ICON_MONOGRAM_FONT_SIZE: f32 = 15.0;
/// `Skills.xaml:151-154` draws the class icon at 50x50. Issue #190 could
/// only fit 40 of that, because `SKILL_HEADER_HEIGHT` was a made-up 56 and
/// 50 would have overflowed its padded content area. Issue #200 measured
/// the reference's header band at 70px instead, so the source's 50 now
/// lands verbatim *and* keeps the "icon exactly fills the padded row"
/// relationship #190 was reaching for
/// (`SKILL_HEADER_HEIGHT - 2 * SKILL_HEADER_PAD_Y` = 50).
pub(crate) const SKILL_HEADER_ICON_SIZE: f32 = 50.0;
pub(crate) const SKILL_HEADER_PAD_X: f32 = 12.0;
pub(crate) const SKILL_HEADER_PAD_Y: f32 = 10.0;
/// The close button's clickable square, which is also the diameter of its
/// circular hover wash (issue #218).
///
/// The reference (`Skills.xaml:214-224`) draws a 16pt `Svg.Close` path with
/// an 8pt margin on every side inside a `ButtonMainStyle` button: 32pt of
/// target around 16pt of glyph. The old 20pt square was the glyph's own box
/// with nothing around it — no radius, no hover fill, no cursor — so there
/// was nothing to aim at and no feedback once you got there. The family's
/// icon buttons use radius = half the side (`MainWindow.xaml:49-55`'s
/// `CornerRadius="18"` on a 36x36 button), i.e. a circle.
pub(crate) const SKILL_CLOSE_HIT_SIZE: f32 = 32.0;
/// The side of the cross's own box inside that target — the reference's
/// `Path … Width="16"`. Painted as two strokes rather than set as text:
/// `U+2715` is not covered by `fonts::bold_family`'s chain and came out as
/// tofu (an empty box), which is what issue #218 called a "square" close
/// button, and the reference's `Svg.Close` is vector art anyway.
pub(crate) const SKILL_CLOSE_GLYPH_SIZE: f32 = 16.0;
/// Stroke weight of those two strokes. `Svg.Close` is a filled path with no
/// nominal weight; 1.6pt is what reads as the same visual density at 16pt
/// against `SKILL_CLOSE_RGB`.
pub(crate) const SKILL_CLOSE_STROKE_WIDTH: f32 = 1.6;
/// The scroll thumb's fill: white at ~20% over the panel, the same read as
/// the reference's thin light thumb. Faded with the rest of the chrome
/// (issue #184).
pub(crate) const SKILL_SCROLL_THUMB_FILL: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
/// The thumb never gets shorter than this, however long the list — a
/// two-pixel nub is not a grabbable or readable position indicator.
pub(crate) const SKILL_SCROLL_THUMB_MIN_HEIGHT: f32 = 24.0;
/// Width of the row list's scrollbar, thumb and track alike (issue #218) —
/// the reference's persistent thin thumb. Also the gutter
/// `skill_rows_content_rect` reserves for it.
pub(crate) const SKILL_SCROLL_BAR_WIDTH: f32 = 6.0;
/// The hover wash `ButtonMainStyle`'s `hl` border flips to on `IsMouseOver`:
/// WPF's 4-digit ARGB `#1fff` — white at alpha `0x11`. Spelled premultiplied
/// because `Color32::from_white_alpha`, which is exactly this, is not `const`.
pub(crate) const SKILL_CLOSE_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0x11, 0x11, 0x11, 0x11);
/// The reference's `CornerRadius="17"` header pill.
pub(crate) const SKILL_PILL_CORNER_RADIUS: u8 = 17;
/// Header pill height, measured at 34px in the reference (issue #200) —
/// exactly `2 * SKILL_PILL_CORNER_RADIUS`, i.e. a true stadium. It used to
/// be derived from the header band instead, which made it 40 tall against a
/// 17 radius: a rounded rectangle with flat sides, not the reference's pill.
pub(crate) const SKILL_PILL_HEIGHT: f32 = 34.0;
/// Gap between two adjacent header pills (issue #254) — the reference's
/// `Margin="0,0,10,0"` on every `Border` in the header's pill `StackPanel`
/// (`Skills.xaml`), which is what separates its Deaths, death-time, aggro
/// and aggro-time capsules.
pub(crate) const SKILL_HEADER_PILL_GAP: f32 = 10.0;
/// The reference's 24pt player name — the one size in this window with no
/// equivalent in the main row scale (`FONT_SIZE_ROW` tops out at 13.0).
pub(crate) const FONT_SIZE_SKILL_HEADER_NAME: f32 = 24.0;

/// Per-tab selection and sort state for one breakdown window (issue #245).
///
/// The sort is kept *per tab*, not per window: the Dps tab's columns and
/// the Heal tab's are largely different, so one shared `SkillSort` would
/// either be reset on every tab switch (losing the ordering the user chose)
/// or left pointing at a column the newly-selected tab does not show. An
/// array indexed by tab makes both impossible — every entry starts on its
/// own tab's `default_sort`, and `sort_mut` can only ever hand back the
/// selected tab's own entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkillTabs {
    pub(crate) selected: skills::SkillTab,
    pub(crate) sorts: [skills::SkillSort; skills::SKILL_TABS.len()],
}

impl Default for SkillTabs {
    fn default() -> Self {
        Self {
            selected: skills::SkillTab::Dps,
            sorts: skills::SKILL_TABS.map(|tab| tab.default_sort()),
        }
    }
}

impl SkillTabs {
    /// This tab's index into `sorts`. `SKILL_TABS` is the single source of
    /// tab order, so the array and the strip can never disagree.
    pub(crate) fn index(tab: skills::SkillTab) -> usize {
        skills::SKILL_TABS
            .iter()
            .position(|t| *t == tab)
            .expect("every SkillTab is listed in SKILL_TABS")
    }

    /// The selected tab's own sort state.
    pub(crate) fn sort_mut(&mut self) -> &mut skills::SkillSort {
        let index = Self::index(self.selected);
        &mut self.sorts[index]
    }
}

/// Initial inner size for a skill breakdown window (issue #181: the viewport
/// is resizable, so this is only the size a newly opened one starts at —
/// `SkillWindowState::size` takes over from there) — wide enough for
/// every `SkillColumn` at its full width plus header padding, tall enough
/// for the header, tab strip and column-header row plus roughly ten rows
/// before the list scrolls. `draw_skill_window` lays everything out from the
/// live viewport rect (`ui.max_rect()`), not this constant, so the content
/// adapts on every resize; `column_anchors_from_widths` scales column widths
/// down when the window is narrower than their sum.
// Issue #192 widened this from 720: the leading icon column adds 32pt of
// column budget, and at 720 the sum of `SkillColumn::width`s would exceed
// the content width, making `column_anchors_from_widths` shrink every
// column to fit rather than laying them out at their stated widths.
// Issue #200 raised the height 520 -> 572: the reference-measured band
// heights (70/22/40 of chrome, 44 per row) make 520 open on only 8.8 rows,
// breaking the "roughly ten rows" promise above. 572 is also the reference
// capture's own content height (928x574 minus its 2px border), where ten
// 44px rows fill y=133..573 exactly.
// Issue #245's live-window pass then measured the *header labels* against
// their columns for the first time and found nine of them overflowing (see
// `SkillColumn::width`), so their widths grew too: the widest tab's sum,
// Dps, went 728 -> 832, and this with it, 760 -> 864 — still 8pt of slack
// over the floor below. It stays inside the reference capture's own
// 928x574 content box, so the window this is modelled on is still the
// wider of the two.
//
// Issue #248 widened it once more, 864 -> 888: re-measuring `SkillColumn::
// Name`'s width off the app's own font (156.9pt for the widest real skill
// name found, see that column's doc comment) raised the widest tab's sum
// again, 832 -> 856, which 864 no longer clears by the required 2 *
// `SKILL_HEADER_PAD_X`. 888 keeps the same 8pt of slack over the new sum
// that 864 kept over the old one, and is still inside that 928x574 box.
pub(crate) const SKILL_WINDOW_SIZE: egui::Vec2 = egui::vec2(888.0, 572.0);
/// Floor on the skill breakdown viewport's inner size (issue #181) so a
/// resize can't shrink it into uselessness — tall enough for the header, tab
/// strip and column-header row plus a couple of rows before the list
/// scrolls, wide enough to fit every column at its stated width.
///
/// Issue #228: the width used to be a flat 360.0, far narrower than the sum
/// of `SkillColumn::width`s (728.0) plus the column header row's left/right
/// `SKILL_HEADER_PAD_X` inset (24.0) — so dragging the window down toward
/// this floor pushed `column_anchors_from_widths` into its proportional
/// shrink path (see its doc comment) while the header labels stayed
/// unclipped at full size, colliding them into unreadable text (e.g.
/// `Damag%Dmg%Max cr…`). Of the fixes the issue lists — eliding column
/// text, progressively dropping columns, or raising this floor — raising
/// the floor is the one taken: it's the smallest change, and every column
/// stays fully legible at every reachable size rather than switching to a
/// second, narrower text-rendering mode. The floor is now exactly that
/// budget, so `column_anchors_from_widths` can still scale a fraction of a
/// point for rounding but never enough to visibly compress a label. See
/// `skill_window_min_width_fits_every_column_at_its_stated_width`.
///
/// Issue #245 widened that budget twice over: first with five more tabs,
/// whose column sets this floor has to clear too (the widest, `Dps`, is
/// what sets it), and then by measuring the header labels themselves — the
/// widths were only ever sized to their columns' *values*, so the labels
/// overlapped even at the initial width. The widest tab's sum went 728 ->
/// 832 and this floor with it, 752 -> 856.
///
/// Issue #248 raised it once more, 856 -> 880, in step with
/// `SKILL_WINDOW_SIZE`: re-measuring the `Name` column grew the widest
/// tab's sum 832 -> 856, and this floor — that sum plus the same 24.0
/// inset — grows with it.
pub(crate) const SKILL_WINDOW_MIN_SIZE: egui::Vec2 = egui::vec2(880.0, 220.0);

/// One open breakdown window's own state (issue #16, D9): its sort column/
/// direction, the screen position it was placed at when opened, and the
/// inner size it is currently shown at. `pos` is computed once, at open
/// time (`skills::place_window`), and never recomputed on a later frame —
/// recomputing it every frame would fight a user actively dragging the
/// window, snapping it back to its dock point on the very next repaint.
/// `size` works the other way around: it starts at `SKILL_WINDOW_SIZE` and
/// then follows the live viewport (`track_skill_window_size`), so a
/// viewport that is torn down and rebuilt reopens at the size the user
/// resized it to instead of snapping back to the constant (issue #181).
/// `source` is which fight the window was opened from, and is what every
/// later frame resolves its row against (issue #216, PR #221 review) — the
/// currently-displayed view is deliberately *not* consulted, or a window
/// opened from Live would silently repaint itself with historical numbers
/// (and back again) as the user moves between the two surfaces, for any uid
/// that happens to exist in both.
pub(crate) struct SkillWindowState {
    /// Issue #245: which breakdown tab is showing, and each tab's own sort.
    pub(crate) tabs: SkillTabs,
    pub(crate) pos: egui::Pos2,
    pub(crate) size: egui::Vec2,
    pub(crate) source: SkillWindowSource,
    /// Issue #218: this window's own in-flight move/resize. Per-window
    /// rather than shared with the root's, because two viewports can be
    /// dragged in two different (non-overlapping) sessions and because
    /// `drive_window_gesture` sends its viewport commands to whichever
    /// context is live — inside `show_viewport_immediate`'s callback that
    /// is this child, not the root.
    pub(crate) gesture: WindowGesture,
}

/// Which fight one breakdown window is showing (issue #216, PR #221
/// review). The historical variant carries the encounter's id rather than a
/// borrowed snapshot so the window's state stays owned and `'static`: the
/// id is matched against whichever encounter is open this frame, and a
/// window whose fight is no longer open (the user went back to the list, or
/// opened a different fight) simply doesn't draw until it is again — the
/// same "skipped for this frame, never closed" tolerance
/// `skill_windows_to_draw` already applies to a uid missing from the rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SkillWindowSource {
    Live,
    History(i64),
}

/// Applies one frame's right-click gesture to the open-window set (D1): a
/// uid not yet open is inserted via `state`; a uid already open is left
/// untouched entirely — re-right-clicking an open row must re-show it, not
/// toggle it closed, and `BTreeMap::entry().or_insert_with` is exactly
/// that (`state` never runs for an already-open uid, so its placement is
/// never recomputed either).
///
/// Returns whether the uid was *already* open, which is the caller's cue to
/// raise and focus that viewport (issue #189). Leaving the map untouched is
/// the whole behaviour for an already-open uid, so without that command the
/// gesture would be a silent no-op — and since the window is no longer
/// always-on-top and has no taskbar entry (`with_taskbar(false)`), a
/// fullscreen game covering it would leave the user with no way back to it,
/// not even to its close glyph.
///
/// The one thing a second right-click *does* change is `source` (issue
/// #216, PR #221 review): right-clicking a historical row for a player who
/// already has a live-opened window is an explicit "show me this fight's
/// breakdown", so the open window retargets onto the fight the gesture came
/// from rather than raising itself still showing the other one. Placement,
/// size and sort are per-window state the user set, and stay untouched.
pub(crate) fn open_skill_window(
    windows: &mut std::collections::BTreeMap<i64, SkillWindowState>,
    uid: i64,
    source: SkillWindowSource,
    state: impl FnOnce() -> SkillWindowState,
) -> bool {
    let already_open = windows.contains_key(&uid);
    // Assigned rather than left to `state`, so the fresh-window and
    // already-open paths cannot disagree about which fight was clicked:
    // whatever `state` built, `source` is the one the gesture came from.
    windows.entry(uid).or_insert_with(state).source = source;
    already_open
}

/// The child viewport id one breakdown window is shown under. A function
/// rather than an inline `from_hash_of` at each site so the open gesture's
/// focus command and the draw loop's viewport can never drift apart onto
/// two different ids for the same uid.
pub(crate) fn skill_viewport_id(uid: i64) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("skills", uid))
}

/// The only two paths that may drop a uid from the open-window set (D2):
/// the in-window `X` and an OS-level close request. Never called for a uid
/// merely missing from the current snapshot — see `skill_windows_to_draw`.
pub(crate) fn close_skill_window(
    windows: &mut std::collections::BTreeMap<i64, SkillWindowState>,
    uid: i64,
) {
    windows.remove(&uid);
}

/// Follows one breakdown window's live inner size (issue #181) so its
/// `ViewportBuilder` reopens it at the size the user left it at rather than
/// at `SKILL_WINDOW_SIZE`. Mirrors `track_window_size`'s job for the root
/// window, `is_plausible_size` guard included; the difference is where the
/// size lands. This one stays in `SkillWindowState` rather than `Settings`
/// because, like `sort` and `pos`, it is per-window state that dies with
/// the window (`close_skill_window`). The child viewport reports its inner
/// rect on every frame, so the update is gated on an actual resize the same
/// way `Settings::with_window_size_if_changed` gates its send: the size is
/// fed straight back into the next frame's builder, and forwarding
/// sub-pixel DPI jitter there would have egui resizing the window at itself
/// on every repaint.
pub(crate) fn track_skill_window_size(size: &mut egui::Vec2, inner_rect: Option<egui::Rect>) {
    let Some(rect) = inner_rect else {
        return;
    };
    let reported = rect.size();
    if !is_plausible_size(reported) {
        return;
    }
    if (size.x - reported.x).abs() < SKILL_WINDOW_SIZE_EPSILON
        && (size.y - reported.y).abs() < SKILL_WINDOW_SIZE_EPSILON
    {
        return;
    }
    *size = reported;
}

/// How far the breakdown viewport has to have resized before
/// `track_skill_window_size` believes it — the same fractional-DPI jitter
/// floor, and the same one-logical-pixel value, `settings.rs`'
/// `SIZE_EPSILON` uses for the root window. Its own constant here because
/// that one is private to `settings.rs` and gates a disk write, while this
/// one gates a viewport command.
pub(crate) const SKILL_WINDOW_SIZE_EPSILON: f32 = 1.0;

/// Which rows *one* breakdown window searches this frame (issue #216, PR
/// #221 review): its own `source`'s, never whichever surface happens to be
/// on screen. A live-opened window keeps reading the live encounter even
/// while History is open, and a window opened from a past fight keeps
/// reading that fight — the two are resolved independently, because a uid
/// (the local player's above all) is routinely present in both and picking
/// by view would swap one window's numbers for the other's mid-flight.
///
/// `None` means "this window has nothing to draw from this frame": its
/// historical fight is no longer the open one (the user went back to the
/// list, opened a different fight, or returned to Live). The window is left
/// open and simply skipped for the frame, the same way
/// `skill_windows_to_draw` skips a uid missing from the rows.
pub(crate) fn skill_window_rows<'a>(
    source: SkillWindowSource,
    live: &'a [PlayerRow],
    history_open: Option<(i64, &'a [PlayerRow])>,
) -> Option<&'a [PlayerRow]> {
    match source {
        SkillWindowSource::Live => Some(live),
        SkillWindowSource::History(id) => {
            history_open.and_then(|(open_id, rows)| (open_id == id).then_some(rows))
        }
    }
}

/// The `(row, uid)` pairs this frame's viewport-draw loop should actually
/// paint: every open uid that still has a row in its own window's source
/// (`skill_window_rows`). A uid with no matching row (a player who dropped
/// off the live encounter, or a stale uid after a reset) is silently
/// excluded from *this frame's* draw pass but left in `windows` — the
/// player can rejoin mid-encounter, and closing on their behalf here would
/// orphan the window state the moment they reappeared. Only
/// `close_skill_window` may remove an entry.
pub(crate) fn skill_windows_to_draw<'a>(
    windows: &std::collections::BTreeMap<i64, SkillWindowState>,
    live: &'a [PlayerRow],
    history_open: Option<(i64, &'a [PlayerRow])>,
) -> Vec<(&'a PlayerRow, i64)> {
    windows
        .iter()
        .filter_map(|(&uid, state)| {
            let rows = skill_window_rows(state.source, live, history_open)?;
            rows.iter().find(|r| r.uid == uid).map(|row| (row, uid))
        })
        .collect()
}

/// Paints one player's skill-breakdown window (issue #16, D5/D9-D12/D14)
/// into its child viewport's root `Ui`. Returns whether the in-window `X`
/// glyph was clicked this frame; the OS close-request check is the call
/// site's job (it reads `ui.ctx().input(|i| i.viewport().close_requested
/// ())` from inside the child callback, which is what reflects the
/// *child's* own close request), so neither close path can leave the
/// window orphaned (D2).
///
/// Painted with explicit rects via `ui.painter()`, mirroring `draw_row`'s
/// style, rather than egui's widget-flow layout: every position is derived
/// live from the viewport's own rect (`ui.max_rect()`) on each frame, so
/// the layout follows the window as the user resizes it (issue #181,
/// `SKILL_WINDOW_SIZE` being only the size it opens at), and the
/// column grid reuses `column_anchors_from_widths`' right-aligned-anchor
/// scheme, the same maths `column_anchors` uses for the main row list —
/// the anchor maths is not re-derived here.
/// The filled box behind the selected tab (issue #200). The reference's tab
/// strip has no band fill of its own: only the selected tab carries
/// `SKILL_PANEL_FILL`, in a box flush with the window's left edge that hugs
/// the label with one `SKILL_HEADER_PAD_X` of padding on each side — the
/// measured `Dps` box runs x 2..51 against a label starting at x≈17.
pub(crate) fn skill_tab_rects(tabs_rect: egui::Rect, text_widths: &[f32]) -> Vec<egui::Rect> {
    let mut x = tabs_rect.left();
    text_widths
        .iter()
        .map(|text_width| {
            let width = text_width + 2.0 * SKILL_HEADER_PAD_X;
            let rect = egui::Rect::from_min_size(
                egui::pos2(x, tabs_rect.top()),
                egui::vec2(width, tabs_rect.height()),
            );
            x += width;
            rect
        })
        .collect()
}

/// The column-header band's rect (issue #200): flush with the window's
/// left/right edges, directly beneath the tab strip, `SKILL_COLUMN_HEADER_
/// HEIGHT` tall. Painted with `SKILL_COLUMN_HEADER_FILL`.
///
/// A tab with no columns (issue #245: `Buff`, which has nothing behind it
/// to tabulate) gets a zero-height band instead of an empty one. The live
/// window painted the full 40pt of `SKILL_COLUMN_HEADER_FILL` there with
/// no labels in it, which read as a stray lighter strip above the
/// explanatory text rather than as a header. Collapsing the rect — rather
/// than special-casing the paint — also lifts `skill_rows_rect`, so the
/// explanation sits where the rows would, directly under the tab strip.
pub(crate) fn skill_column_header_rect(
    rect: egui::Rect,
    tabs_rect: egui::Rect,
    columns: &[skills::SkillColumn],
) -> egui::Rect {
    let height = if columns.is_empty() {
        0.0
    } else {
        SKILL_COLUMN_HEADER_HEIGHT
    };
    egui::Rect::from_min_size(
        egui::pos2(rect.left(), tabs_rect.bottom()),
        egui::vec2(rect.width(), height),
    )
}

/// The scrollable row-list band's rect (issue #200): everything below the
/// column header down to the window's bottom edge. Painted with
/// `SKILL_PANEL_FILL`, not the window's `SKILL_CHROME_FILL`.
pub(crate) fn skill_rows_rect(rect: egui::Rect, col_header_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(rect.left(), col_header_rect.bottom()), rect.max)
}

/// The message painted where the row list would go when a breakdown window
/// has nothing to show (issue #216) — which of the two it is depends on the
/// window's source, not just on the row count (PR #221 review).
///
/// A historical fight's `PlayerRow::skills` is empty *for good*: schema v2
/// persists per-skill totals (issue #222), so a fight recorded by this build
/// has its breakdown, and an empty one is a fight saved before v2 or a player
/// who never landed a hit — either way nothing will ever populate it now, and
/// naming that is what keeps the window from reading as silent breakage. A
/// live row's empty `skills` means only "not yet": the dungeon
/// roster preload (`encounter::apply_player`) puts a party member in the
/// snapshot with an empty skill map before their first hit lands, and a
/// healer can sit there for a whole fight — telling that user "nothing was
/// recorded for this fight" would be plainly wrong while the fight is still
/// running, so the live wording promises the rows are coming.
pub(crate) fn skill_window_empty_message(
    source: SkillWindowSource,
    tab: skills::SkillTab,
    skill_row_count: usize,
) -> Option<&'static str> {
    if skill_row_count > 0 {
        return None;
    }
    // Issue #245: a tab nothing feeds says so, whatever the window's
    // source — "No damage recorded yet" would read as "this fight had
    // none", when the truth is that this build never looks.
    if let Some(untracked) = tab.untracked_message() {
        return Some(untracked);
    }
    Some(match source {
        // The breakdowns issue #245 adds are live-only: the on-disk fight
        // history serialises `PlayerRow::skills` and nothing else, so a
        // saved fight has no heal/dealt/received rows to hand back and the
        // existing history wording covers all four alike.
        SkillWindowSource::Live => match tab {
            skills::SkillTab::Dps => "No damage recorded yet",
            skills::SkillTab::Heal => "No healing recorded yet",
            skills::SkillTab::Dealt => "Nothing dealt yet",
            skills::SkillTab::Received => "Nothing received yet",
            skills::SkillTab::Casts => "Nothing cast yet",
            skills::SkillTab::Buff => "Nothing recorded yet",
        },
        SkillWindowSource::History(_) => "No per-skill data recorded for this fight",
    })
}

/// The header band: full width, `SKILL_HEADER_HEIGHT` tall, flush with the
/// window's top. Pulled out of `draw_skill_window` (issue #218) so the drag
/// band and close button derived from it are testable without a live `Ui`.
pub(crate) fn skill_header_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), SKILL_HEADER_HEIGHT))
}

/// The window's drag surface (issue #218): the header band only, inset by
/// `RESIZE_EDGE` on the three window edges it touches.
///
/// It used to be `ui.max_rect()` — the whole viewport — which broke two
/// things at once. It covered the eight `resize_zones` strips, so the
/// borderless window had no reachable resize grip anywhere; and it covered
/// the row list, where a `Sense::drag()` strands egui's `dragged_id()` and
/// so gates off *all* mouse-wheel scrolling
/// (`scroll_area.rs`' `is_hovering_outer_rect`, which is
/// `… && ui.ctx().dragged_id().is_none()`).
///
/// Same shape and same reasoning as the main window's header band — see the
/// "a drag surface spanning it would win the hit test and swallow every
/// north-edge resize" note there. No bottom inset is needed: the south
/// strip is a window height away.
pub(crate) fn skill_drag_band(header_rect: egui::Rect) -> egui::Rect {
    let mut band = header_rect;
    band.min.y += RESIZE_EDGE;
    band.min.x += RESIZE_EDGE;
    band.max.x -= RESIZE_EDGE;
    band
}

/// The close button's hit square (issue #218), top-right of the window
/// inside the header's padding. `SKILL_CLOSE_HIT_SIZE` wide, so it is also
/// the bounding box of the circular hover wash painted at its centre.
pub(crate) fn skill_close_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - SKILL_HEADER_PAD_X - SKILL_CLOSE_HIT_SIZE,
            rect.top() + SKILL_HEADER_PAD_Y,
        ),
        egui::Vec2::splat(SKILL_CLOSE_HIT_SIZE),
    )
}

/// The close cross's two strokes, as endpoint pairs: the diagonals of a
/// `SKILL_CLOSE_GLYPH_SIZE` box centred in the button (issue #218). Pure so
/// the shape survives without a font and is checkable without a `Ui`.
pub(crate) fn skill_close_cross(close_rect: egui::Rect) -> [[egui::Pos2; 2]; 2] {
    let arms = egui::Rect::from_center_size(
        close_rect.center(),
        egui::Vec2::splat(SKILL_CLOSE_GLYPH_SIZE),
    );
    [
        [arms.left_top(), arms.right_bottom()],
        [arms.right_top(), arms.left_bottom()],
    ]
}

/// The rect the row list lays its rows out in: the rows band minus the
/// scrollbar's gutter (issue #218).
///
/// The rows used to be allocated at the full band width, which put every
/// row's own painting across the strip the bar lives in — a solid bar has
/// to take its width out of the content, and content that ignores that just
/// paints over it.
pub(crate) fn skill_rows_content_rect(rows_rect: egui::Rect) -> egui::Rect {
    let mut content = rows_rect;
    content.max.x -= SKILL_SCROLL_BAR_WIDTH;
    content
}

/// The scroll thumb for a row list of `content_height` scrolled to
/// `offset_y`, or `None` when the list fits and needs none.
///
/// Painted by hand (issue #218) rather than left to egui's own scroll bar:
/// that bar never reached the screen here — with `ScrollStyle::solid()` set
/// on the list's `Ui` it still painted no track and no handle, headless or
/// live — and the reference's persistent thin thumb is a fixed piece of
/// chrome anyway, not egui's hover-faded floating bar. Driven off the
/// `ScrollAreaOutput` egui already hands back, so it cannot drift out of
/// step with where the list actually is.
pub(crate) fn skill_scroll_thumb(
    rows_rect: egui::Rect,
    content_height: f32,
    offset_y: f32,
) -> Option<egui::Rect> {
    let track = rows_rect.height();
    if content_height <= track {
        return None;
    }
    // Proportional to how much of the list is on screen, which is what
    // makes the thumb read as "how far through am I" and not just "there is
    // more".
    let thumb_height = (track * track / content_height).clamp(SKILL_SCROLL_THUMB_MIN_HEIGHT, track);
    let travel = (offset_y / (content_height - track)).clamp(0.0, 1.0);
    Some(egui::Rect::from_min_size(
        egui::pos2(
            rows_rect.right() - SKILL_SCROLL_BAR_WIDTH,
            rows_rect.top() + (track - thumb_height) * travel,
        ),
        egui::vec2(SKILL_SCROLL_BAR_WIDTH, thumb_height),
    ))
}

/// Gap between the player name's right edge and the Deaths pill's left edge
/// (issue #246). Measured directly off `docs/reference/shinra-skills-ex.webp`:
/// scanning the header row (y=36) for `"Ranmori"`'s last glyph stroke and
/// the pill's rounded-rect fill finds the name ending at x=156 and the pill
/// starting at x=182, a 26px gap. That lines up with `Skills.xaml`'s own
/// source: the pill cluster's `StackPanel` (`Skills.xaml:172-176`) carries
/// `Margin="20,0"` inside the header's flexible middle grid column, right
/// after the name — the 6px difference from the pixel read is font-metric
/// slop between the reference's rendered text and the raw XAML number, not
/// a second, disagreeing source.
pub(crate) const SKILL_DEATHS_PILL_GAP: f32 = 26.0;

/// Where the header's pill cluster starts — the Deaths pill's left edge,
/// and with it every pill that follows it: immediately after the player
/// name (issue #246), not right-aligned against the close button. The old
/// close-relative placement tucked the cluster under the close button; the
/// reference (`Skills.xaml:172-176`) instead sits its whole pill cluster in
/// the header's flexible middle column, directly following the name — see
/// `SKILL_DEATHS_PILL_GAP` for the sourcing.
///
/// `PlayerRow::name` is an unbounded, network-decoded string (never
/// truncated — the same "paint it in full" decision the row list's own name
/// column makes at issue #168), so a long enough name can push
/// `name_right + SKILL_DEATHS_PILL_GAP` past the close button entirely.
/// Clamped so `left + cluster_width` never crosses `close_left -
/// SKILL_HEADER_PAD_X` — which is exactly where the pre-#246 close-relative
/// formula put the cluster by construction, so that placement survives as
/// this one's ceiling: the gap-after-name position is only ever the
/// *preferred* one, never unconditional.
///
/// Takes the *cluster's* width, not the Deaths pill's alone (issue #254):
/// the death-time pill sits to the Deaths pill's right, so clamping on the
/// first pill only would push the last one under the close button.
pub(crate) fn skill_deaths_pill_left(name_right: f32, close_left: f32, cluster_width: f32) -> f32 {
    let preferred = name_right + SKILL_DEATHS_PILL_GAP;
    let max_left = close_left - SKILL_HEADER_PAD_X - cluster_width;
    preferred.min(max_left)
}

/// Total width of the header's pill cluster: the Deaths pill, plus the
/// death-time pill and the gap before it when there is one to draw (issue
/// #254). One helper so the cluster is measured in exactly one place
/// whether it holds one pill or two.
pub(crate) fn skill_header_pill_cluster_width(
    deaths_width: f32,
    death_time_width: Option<f32>,
) -> f32 {
    deaths_width + death_time_width.map_or(0.0, |w| SKILL_HEADER_PILL_GAP + w)
}

/// The death-time pill's text (issue #254), or `None` when there is no pill
/// to draw at all.
///
/// - `None` in, `None` out: a history row carries no death time (see
///   `PlayerRow::dead_ms`), and an empty capsule would read as "nobody was
///   on the floor" rather than "not recorded".
/// - Zero renders as a bare `00:00`. Nobody died, so the figure is exact,
///   and marking it as an estimate would be the lie — the tilde below is
///   reserved for numbers that actually are estimated.
/// - Anything else takes a `~` prefix: `~00:12`. The revive edge feeding
///   this total is *inferred* from the player's next action, not observed
///   (`PlayerStats::dead_ms`), so the number is real but biased high, and
///   it sits one pill away from the exact Deaths counter. The tilde is the
///   cheapest honest marker available here: these header capsules are bare
///   painted ovals with no widget behind them — the header's whole width is
///   a drag band — so a tooltip would mean carving a hover region out of
///   that band, and a dimmer tint would read as "less important" rather
///   than "less certain", on top of being invisible to anyone who never
///   sees the two side by side.
///
/// Formatted `mm:ss` through `fmt_duration`, matching the reference's
/// `interval.ToString(@"mm\:ss")` (`Skills.xaml.cs`) and the fight timer in
/// the main window's header.
pub(crate) fn skill_death_time_text(dead_ms: Option<u64>) -> Option<String> {
    let ms = dead_ms?;
    Some(if ms == 0 {
        fmt_duration(0)
    } else {
        format!("~{}", fmt_duration(ms))
    })
}

pub(crate) fn draw_skill_window(
    ui: &mut egui::Ui,
    row: &PlayerRow,
    tabs: &mut SkillTabs,
    source: SkillWindowSource,
    icons: &Icons,
    opacity: Opacity,
    gesture: &mut WindowGesture,
) -> bool {
    let rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, opacity.apply(SKILL_CHROME_FILL));

    // Issue #218: this window is `with_decorations(false)` like the root, so
    // winit cancels `WS_SIZEBOX` and hands back no OS resize frame — the
    // `with_resizable(true)` on its builder is dead. It supplies its own
    // grips exactly as the root window does. Registered first so the header
    // widgets below win the pixels they overlap; egui gives interaction
    // priority to whatever was registered later. Its double-click return
    // (issue #300) is discarded here — a breakdown viewport has no row-count
    // preset of its own to snap to.
    let _ = draw_resize_handles(ui, &ctx, gesture, ("skill", row.uid));

    // -- header: class icon, player name, the pill cluster (D10) ---------
    let header_rect = skill_header_rect(rect);

    // Dragging the header moves the window (the child viewport has no OS
    // titlebar, D2's `with_decorations(false)`). Issue #218: this used to
    // sense the whole of `rect` and fire `ViewportCommand::StartDrag`, and
    // both halves were bugs. The full-viewport rect buried the resize
    // strips and, worse, put a `Sense::drag()` over the row list, where a
    // stranded `dragged_id()` gates off all wheel scrolling — see
    // `skill_drag_band`. And `StartDrag` enters Windows' `SC_MOVE` modal
    // loop, which eats the `WM_LBUTTONUP` that would have cleared that drag
    // state, on top of being the one place Aero Snap engages (issue #11).
    // So this goes through `WindowGesture` like the root window's header
    // does. The reposition exemption that gesture holds is root-HWND-only
    // and simply inert here — a missing Snap-blocker exemption for a window
    // the Snap blocker never sees, not a correctness gap.
    let drag = ui.interact(
        skill_drag_band(header_rect),
        ui.id().with(("skill_drag", row.uid)),
        egui::Sense::drag(),
    );
    if drag.drag_started_by(egui::PointerButton::Primary) {
        begin_window_gesture(&ctx, gesture, GestureKind::Move);
    }

    let icon_rect = egui::Rect::from_center_size(
        header_rect.left_center()
            + egui::vec2(SKILL_HEADER_PAD_X + SKILL_HEADER_ICON_SIZE / 2.0, 0.0),
        egui::Vec2::splat(SKILL_HEADER_ICON_SIZE),
    );
    if let Some(texture) = row.class.and_then(|class| icons.classes.get(class)) {
        painter.image(texture.id(), icon_rect, UV_FULL, CLASS_ICON_TINT);
    }
    // Issue #246 anchors the pill cluster on the player name's right edge,
    // so the name has to be *measured* before the cluster can be placed —
    // while issue #270's clip below bounds the name against that cluster.
    // Laying the galley out here, rather than reading the rect
    // `paint_text` hands back, is what breaks the circle between the two.
    let name_left = icon_rect.right() + SKILL_HEADER_PAD_X;
    let name_right = name_left
        + painter
            .layout_no_wrap(
                row.name.clone(),
                regular(FONT_SIZE_SKILL_HEADER_NAME),
                egui::Color32::WHITE,
            )
            .rect
            .width();

    // D10: the Deaths pill, and beside it issue #254's death-time pill —
    // the first two of the reference's four-capsule cluster
    // (`Skills.xaml`), in its order and with its 10pt gap. The remaining
    // two (aggro count, aggro time) need threat data this decoder never
    // captures, so they stay out: rendering them would be inventing
    // numbers rather than reporting them.
    //
    // Sized and placed *before* the name is painted (issue #270 follow-up)
    // so `cluster_left` exists in time to clip the name against it — see
    // below.
    let deaths_text = row.deaths.to_string();
    let deaths_pill = StatPill {
        value: &deaths_text,
        icon: icons.glyphs.get(GlyphIcon::Skull).map(|t| t.id()),
        icon_side: COUNTER_GLYPH_SIDE,
        size: FONT_SIZE_PILL_VALUE,
        value_color: egui::Color32::WHITE,
        icon_color: COUNTER_ICON_COLOR,
        icon_first: true,
        corner_radius: egui::CornerRadius::same(SKILL_PILL_CORNER_RADIUS),
        // Issue #184: every filled surface in this window takes `opacity`,
        // the pill included — it used to be the one chrome element that
        // stayed solid while the window around it faded.
        fill: opacity.apply(SKILL_PANEL_FILL),
        stroke: None,
    };
    // Issue #254: the same chrome as the Deaths pill — one cluster, one
    // look — led by the stopwatch the main header's fight timer already
    // uses, since neither vendored icon set has the reference's dedicated
    // death-time mark and a second clock reads as "time" on sight. Absent
    // (not empty) when the row carries no measured death time; see
    // `skill_death_time_text` for the `~` on an estimated total.
    let death_time_text = skill_death_time_text(row.dead_ms);
    let death_time_pill = death_time_text.as_ref().map(|value| StatPill {
        value,
        icon: icons.glyphs.get(GlyphIcon::Timer).map(|t| t.id()),
        ..deaths_pill
    });

    let deaths_text_size = pill_text_size(&painter, &deaths_pill);
    let deaths_pill_size = pill_size(deaths_text_size, deaths_pill.icon_side, SKILL_PILL_HEIGHT);
    let death_time_sizes = death_time_pill.as_ref().map(|pill| {
        let text_size = pill_text_size(&painter, pill);
        (
            text_size,
            pill_size(text_size, pill.icon_side, SKILL_PILL_HEIGHT),
        )
    });
    // The close button's rect is derived here, ahead of its own paint
    // below, so the cluster can be clamped clear of it — see
    // `skill_deaths_pill_left`.
    let close_rect = skill_close_rect(rect);
    let cluster_left = skill_deaths_pill_left(
        name_right,
        close_rect.left(),
        skill_header_pill_cluster_width(
            deaths_pill_size.x,
            death_time_sizes.map(|(_, size)| size.x),
        ),
    );

    // Issue #270 follow-up: the name used to paint with no clip, max-width
    // or elision at all, so a long enough player name ran underneath the
    // pill cluster — pre-existing, but #270 made it materially worse
    // (the new death-time pill costs another `SKILL_HEADER_PILL_GAP` +
    // pill width of cluster real estate, moving `cluster_left` further
    // left on every row that carries one). Clipped, not elided: the same
    // "cut off, never truncated to `…`" choice `column_clip_rect` makes for
    // the main meter's stat columns (an overlong value there is bounded by
    // its slot rather than losing characters to an ellipsis), rather than
    // the "leave it unclipped" call issues #26/#168 made for the main
    // meter's *name* column — that one gets away with it because the stat
    // columns painted after it are opaque at every opacity setting, while
    // this cluster's pill fill is `opacity`-scaled and would otherwise let
    // an overrun name bleed through it. One `SKILL_HEADER_PAD_X` of gap
    // before the cluster, matching the pad already used everywhere else in
    // this header.
    let name_painter = painter.with_clip_rect(egui::Rect::from_min_max(
        header_rect.left_top(),
        egui::pos2(cluster_left - SKILL_HEADER_PAD_X, header_rect.bottom()),
    ));
    paint_text(
        &name_painter,
        egui::pos2(name_left, header_rect.center().y),
        egui::Align2::LEFT_CENTER,
        &row.name,
        regular(FONT_SIZE_SKILL_HEADER_NAME),
        egui::Color32::WHITE,
        false,
    );

    let deaths_pill_rect = egui::Rect::from_min_size(
        egui::pos2(
            cluster_left,
            header_rect.center().y - deaths_pill_size.y / 2.0,
        ),
        deaths_pill_size,
    );
    paint_stat_pill(&painter, deaths_pill_rect, deaths_text_size, &deaths_pill);
    if let (Some(pill), Some((text_size, size))) = (&death_time_pill, death_time_sizes) {
        let rect = egui::Rect::from_min_size(
            egui::pos2(
                deaths_pill_rect.right() + SKILL_HEADER_PILL_GAP,
                header_rect.center().y - size.y / 2.0,
            ),
            size,
        );
        paint_stat_pill(&painter, rect, text_size, pill);
    }

    // -- close glyph (D2): the only in-window way to close ---------------
    // Issue #218: interacted *before* it is painted, because the hover wash
    // is part of the paint — a 32pt circle behind the glyph, matching the
    // reference's `ButtonMainStyle` (`#1fff` on `IsMouseOver`, radius =
    // half the side) — and a pointing-hand cursor. The glyph itself used to
    // be the whole button: a 20pt square with no radius, no hover feedback
    // and no cursor change, so nothing about it read as clickable.
    let close = ui.interact(
        close_rect,
        ui.id().with(("skill_close", row.uid)),
        egui::Sense::click(),
    );
    if close.hovered() {
        painter.circle_filled(
            close_rect.center(),
            SKILL_CLOSE_HIT_SIZE / 2.0,
            SKILL_CLOSE_HOVER_FILL,
        );
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Two strokes, not a `\u{2715}`: that codepoint is outside every font in
    // `fonts::bold_family`'s chain, so it rendered as tofu — the empty box
    // issue #218 reported as a "square" close button. The reference's
    // `Svg.Close` is vector art too.
    let stroke = egui::Stroke::new(SKILL_CLOSE_STROKE_WIDTH, SKILL_CLOSE_RGB);
    for [from, to] in skill_close_cross(close_rect) {
        painter.line_segment([from, to], stroke);
    }
    let close_clicked = close.clicked();

    // -- tab strip (D11, issue #245) --------------------------------------
    // The reference's seven tabs (`Skills.xaml:227-236`) minus `Mana`,
    // which BPSR's packet stream has no resource to fill — see
    // `skills::SkillTab`. Clicking one selects it; each keeps its own sort.
    let tabs_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), header_rect.bottom()),
        egui::vec2(rect.width(), SKILL_TAB_HEIGHT),
    );
    // Issue #200: the strip itself is *not* a filled band. At y=80 in the
    // reference the pixels under the unselected tabs (x=700) match the
    // header band exactly, while only the selected `Dps` tab (x 2..51)
    // carries the lighter `#212127` box. Filling the whole strip made the
    // window read as a two-tone sandwich instead of a tab row.
    let tab_font = bold(FONT_SIZE_ROW);
    let tab_text_widths: Vec<f32> = skills::SKILL_TABS
        .iter()
        .map(|tab| {
            painter
                .layout_no_wrap(
                    tab.label().to_owned(),
                    tab_font.clone(),
                    egui::Color32::WHITE,
                )
                .size()
                .x
        })
        .collect();
    for (i, (tab, tab_rect)) in skills::SKILL_TABS
        .iter()
        .zip(skill_tab_rects(tabs_rect, &tab_text_widths))
        .enumerate()
    {
        let selected = tabs.selected == *tab;
        let response = ui.interact(
            tab_rect,
            ui.id().with(("skill_tab", row.uid, i)),
            egui::Sense::click(),
        );
        if response.clicked() {
            tabs.selected = *tab;
        }
        if response.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if selected || response.hovered() {
            painter.rect_filled(tab_rect, 0.0, opacity.apply(SKILL_PANEL_FILL));
        }
        // An untracked tab (issue #245: Buff) is drawn muted
        // rather than hidden — the reference has it, this build cannot
        // fill it, and the body says why once it is opened. Hiding it
        // would leave the gap unexplained; greying it out of clicking
        // would leave the explanation unreachable.
        let color = match (selected, tab.is_tracked()) {
            (true, true) => egui::Color32::WHITE,
            (false, true) => SKILL_TAB_IDLE_RGB,
            (true, false) => SKILL_TAB_IDLE_RGB,
            (false, false) => SKILL_TAB_UNTRACKED_RGB,
        };
        paint_text(
            &painter,
            tab_rect.left_center() + egui::vec2(SKILL_HEADER_PAD_X, 0.0),
            egui::Align2::LEFT_CENTER,
            tab.label(),
            tab_font.clone(),
            color,
            true,
        );
    }
    let tab = tabs.selected;
    let columns = tab.columns();
    let sort = tabs.sort_mut();

    // -- column header row: click (either button, D9) toggles sort -------
    let col_header_rect = skill_column_header_rect(rect, tabs_rect, columns);
    painter.rect_filled(
        col_header_rect,
        0.0,
        opacity.apply(SKILL_COLUMN_HEADER_FILL),
    );
    // Issue #245: the narrower tabs would otherwise pack against the
    // window's right edge (`column_anchors_from_widths` lays out
    // right-to-left), so any slack goes to the `Name` column.
    let content_left = col_header_rect.left() + SKILL_HEADER_PAD_X;
    let content_right = col_header_rect.right() - SKILL_HEADER_PAD_X;
    let widths = skills::column_widths(columns, content_right - content_left);
    let anchors = column_anchors_from_widths(content_left, content_right, &widths, 0.0);
    for (i, ((&anchor_x, kind), &width)) in anchors
        .iter()
        .zip(columns.iter())
        .zip(widths.iter())
        .enumerate()
    {
        let cell = egui::Rect::from_min_max(
            egui::pos2(anchor_x - width, col_header_rect.top()),
            egui::pos2(anchor_x, col_header_rect.bottom()),
        );
        let response = ui.interact(
            cell,
            ui.id().with(("skill_col_header", row.uid, i)),
            egui::Sense::click(),
        );
        if kind.sortable() && (response.clicked() || response.secondary_clicked()) {
            sort.toggle(*kind);
        }
        let label = sort.header_label(*kind);
        let (align, pos) = if kind.left_aligned() {
            (egui::Align2::LEFT_CENTER, cell.left_center())
        } else {
            (egui::Align2::RIGHT_CENTER, cell.right_center())
        };
        paint_text(
            &painter,
            pos,
            align,
            &label,
            bold(FONT_SIZE_ROW),
            SKILL_HEADER_RGB,
            true,
        );
    }

    // -- rows: sorted per-window (D9), scrollable, no grouping (D12) -----
    // BPSR's skill ids are flat — there is no "short name" to group
    // sub-skills under, so unlike the reference's expander rows this is
    // deliberately one row per skill id with no expand/collapse tier.
    let rows_rect = skill_rows_rect(rect, col_header_rect);
    // Issue #200: the row list sits on the panel fill, not the window's
    // chrome fill. Measured at x=860 the reference's rows band is exactly
    // `SKILL_PANEL_FILL - SKILL_CHROME_FILL` (16 per channel) brighter than
    // the header above it.
    painter.rect_filled(rows_rect, 0.0, opacity.apply(SKILL_PANEL_FILL));
    let mut skill_rows = tab.rows(row).to_vec();
    skills::sort_rows(&mut skill_rows, *sort);

    // Issue #216: an empty row list gets a message in place of the rows,
    // worded for where the window's data comes from — a historical fight
    // never has per-skill rows at all, a live one just doesn't have them
    // yet (see `skill_window_empty_message`).
    if let Some(message) = skill_window_empty_message(source, tab, skill_rows.len()) {
        paint_text(
            &painter,
            rows_rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            regular(FONT_SIZE_ROW),
            egui::Color32::GRAY,
            false,
        );
        // Issue #299: this used to return before `drive_window_gesture`
        // ran. The Buff tab's rows are *always* empty (`SkillTab::rows`
        // hands it `&[]` unconditionally), so this branch is taken on
        // every single frame the Buff tab is selected — meaning a
        // move/resize gesture begun while it was showing was never driven
        // to completion: not moved, not ended when the pointer let go,
        // just stranded until the user switched to a tab with rows, which
        // then applied the whole stale delta in one jump. Switching tabs
        // must only change the displayed breakdown, never a live gesture
        // (or anything else) in flight.
        drive_window_gesture(&ctx, gesture, SKILL_WINDOW_MIN_SIZE);
        return close_clicked;
    }

    let mut rows_ui = ui.new_child(egui::UiBuilder::new().max_rect(rows_rect));
    // Issue #218: rows are laid out inside the thumb's gutter, never across
    // it, so no row's hover fill or clipped cell paints over the thumb.
    let rows_content_rect = skill_rows_content_rect(rows_rect);
    let scroll = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        // egui's own bar is suppressed rather than styled: it painted
        // nothing here either way (see `skill_scroll_thumb`), and leaving
        // it enabled would silently take a second gutter's width out of
        // the content.
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut rows_ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for skill in &skill_rows {
                // Resolved once per row (table lookup + allocation) and
                // reused below for both the icon column's monogram and the
                // Name column's cell text, rather than resolving it twice.
                let display_name = skills::skill_display_name(skill.skill_id);
                let (skill_rect, response) = ui.allocate_exact_size(
                    egui::vec2(rows_content_rect.width(), SKILL_ROW_HEIGHT),
                    egui::Sense::hover(),
                );
                if response.hovered() {
                    // Issue #184: fades with the rest of the chrome, same
                    // as the tab strip and the header pill.
                    ui.painter()
                        .rect_filled(skill_rect, 0.0, opacity.apply(SKILL_ROW_HOVER_FILL));
                }
                for ((&anchor_x, kind), &width) in
                    anchors.iter().zip(columns.iter()).zip(widths.iter())
                {
                    let clip = egui::Rect::from_min_max(
                        egui::pos2(anchor_x - width, skill_rect.top()),
                        egui::pos2(anchor_x, skill_rect.bottom()),
                    );
                    let cell_painter = ui.painter().with_clip_rect(clip);
                    // Issue #192: the leading icon column paints a texture,
                    // not text. Left-aligned inside its cell so the icons
                    // line up with each other and sit a fixed gap from the
                    // skill name, exactly as in the reference. A skill the
                    // name tables know no icon for, an icon whose PNG is not
                    // vendored here, and one that failed to decode all land
                    // on the same no-texture branch — one degrade path,
                    // never a panic, the same shape `ImagineIcons`' empty
                    // slot uses. Issue #275: that branch used to be one flat
                    // `SKILL_ICON_EMPTY` disc for every such id, painting the
                    // capture's single largest damage source identically to
                    // a 0.01% tick three rows below it. It now paints a
                    // monogram placeholder — see `skills::skill_monogram`
                    // and `paint_skill_icon_placeholder` — keyed off the
                    // *name* every one of those ids already resolves to,
                    // and only falls through to the old blank disc for a
                    // name with no derivable glyph at all (see
                    // `skills::skill_monogram`'s doc comment).
                    if *kind == skills::SkillColumn::Icon {
                        let center =
                            egui::pos2(clip.left() + SKILL_ICON_SIZE / 2.0, clip.center().y);
                        match skills::skill_icon_basename(skill.skill_id)
                            .and_then(|basename| icons.skills.get(basename))
                        {
                            Some(texture) => {
                                cell_painter.image(
                                    texture.id(),
                                    egui::Rect::from_center_size(
                                        center,
                                        egui::Vec2::splat(SKILL_ICON_SIZE),
                                    ),
                                    UV_FULL,
                                    CLASS_ICON_TINT,
                                );
                            }
                            None => paint_skill_icon_placeholder(
                                &cell_painter,
                                center,
                                skill.skill_id,
                                &display_name,
                                opacity,
                            ),
                        }
                        continue;
                    }
                    let (align, pos) = if kind.left_aligned() {
                        (egui::Align2::LEFT_CENTER, clip.left_center())
                    } else {
                        (egui::Align2::RIGHT_CENTER, clip.right_center())
                    };
                    let text = if *kind == skills::SkillColumn::Name {
                        display_name.clone()
                    } else {
                        kind.text(skill)
                    };
                    paint_text(
                        &cell_painter,
                        pos,
                        align,
                        &text,
                        regular(FONT_SIZE_ROW),
                        egui::Color32::WHITE,
                        false,
                    );
                }
            }
        });

    // Painted after the list, so it sits over the rows rather than under
    // them, and from the scroll area's own reported geometry.
    if let Some(thumb) = skill_scroll_thumb(rows_rect, scroll.content_size.y, scroll.state.offset.y)
    {
        painter.rect_filled(
            thumb,
            egui::CornerRadius::same((SKILL_SCROLL_BAR_WIDTH / 2.0) as u8),
            opacity.apply(SKILL_SCROLL_THUMB_FILL),
        );
    }

    // Last, so the header band and the eight resize handles above have all
    // had their frame to start a gesture (issue #218) — the same ordering
    // `OverlayApp::ui` uses for the root window. Inside
    // `show_viewport_immediate`'s callback `ctx`'s viewport commands and
    // input both address *this* child, so the same driver moves and resizes
    // it without knowing it is not the root.
    drive_window_gesture(&ctx, gesture, SKILL_WINDOW_MIN_SIZE);

    close_clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
    /// Const-only regression pin for `SKILL_ICON_PLACEHOLDER_INSET`'s
    /// relationship to `SKILL_ICON_SIZE` — it does not touch anything
    /// `paint_skill_icon_placeholder` paints (see
    /// `placeholder_disc_paints_at_the_inset_radius_inside_the_icon_slot`
    /// for that). Issue #281: the placeholder disc used to fill the full
    /// `SKILL_ICON_SIZE` slot, reading heavier than real vendored art's
    /// ~26x28 visible footprint (its own transparent padding) in the same
    /// 38x38 slot. Pins the inset to the small "~2-3px" range that closes
    /// that gap without shrinking the disc out of the same size class as
    /// real icon art, and pins `SKILL_ICON_PLACEHOLDER_RADIUS`'s formula so
    /// a hand-edit of either constant is caught here first.
    #[test]
    fn placeholder_disc_inset_pins_the_slot_relationship() {
        const { assert!(SKILL_ICON_PLACEHOLDER_INSET >= 2.0 && SKILL_ICON_PLACEHOLDER_INSET <= 3.0) };
        const {
            assert!(
                SKILL_ICON_PLACEHOLDER_RADIUS
                    == SKILL_ICON_SIZE / 2.0 - SKILL_ICON_PLACEHOLDER_INSET
            )
        };
        const { assert!(SKILL_ICON_PLACEHOLDER_RADIUS < SKILL_ICON_SIZE / 2.0) };
    }

    /// Renders a real skill row through `draw_skill_window` and reads the
    /// disc `paint_skill_icon_placeholder` actually painted back out of the
    /// frame's `Shape::Circle`s — `placeholder_disc_inset_pins_the_slot_
    /// relationship` only checks the constants' own arithmetic against each
    /// other, which can't catch either of `paint_skill_icon_placeholder`'s
    /// two `circle_filled` call sites drifting from
    /// `SKILL_ICON_PLACEHOLDER_RADIUS` (e.g. a copy-pasted literal).
    #[test]
    fn placeholder_disc_paints_at_the_inset_radius_inside_the_icon_slot() {
        let row = PlayerRow {
            // A negative id: `skill_icon_basename` is keyed by `u32`, so a
            // negative one can never resolve to a vendored icon and always
            // takes the placeholder branch — unlike a real id, which could
            // start shipping an icon later and silently switch this test
            // onto the textured branch instead.
            skills: vec![sample_skill_row(-1)],
            ..sample_row(None)
        };
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_SIZE);
        let mut tabs_state = SkillTabs::default();

        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_skill_window(
                    ui,
                    &row,
                    &mut tabs_state,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
                    &mut WindowGesture::default(),
                );
            },
        );
        let mut circles = Vec::new();
        for clipped in &output.shapes {
            collect_circle_geometry(&clipped.shape, &mut circles);
        }
        output.drop_without_applying_deltas();

        // The same geometry `draw_skill_window` derives the icon cell's
        // center from (issue #200's `skill_header_rect`/
        // `skill_column_header_rect`/`skill_rows_rect`, and the shared
        // column `anchors` the row loop reuses from the column-header
        // row — over issue #245's `skills::column_widths`, which hands the
        // content's slack to `Name` alone, so replicating the layout with
        // the raw `SkillColumn::width`s would land the icon elsewhere)
        // — replicated here rather than pulled into a shared helper,
        // since nothing else needs "where does the icon column sit"
        // outside this one check.
        let header = skill_header_rect(screen_rect);
        let tabs = egui::Rect::from_min_size(
            egui::pos2(screen_rect.left(), header.bottom()),
            egui::vec2(screen_rect.width(), SKILL_TAB_HEIGHT),
        );
        // The default tab (`SkillTabs::default`) is the one the frame above
        // painted, and issue #245 made each tab's column list its own.
        let columns = skills::SkillTab::Dps.columns();
        let col_header = skill_column_header_rect(screen_rect, tabs, columns);
        let rows_rect = skill_rows_rect(screen_rect, col_header);
        let content_left = col_header.left() + SKILL_HEADER_PAD_X;
        let content_right = col_header.right() - SKILL_HEADER_PAD_X;
        let widths = skills::column_widths(columns, content_right - content_left);
        let anchors = column_anchors_from_widths(content_left, content_right, &widths, 0.0);
        let icon_index = columns
            .iter()
            .position(|k| *k == skills::SkillColumn::Icon)
            .expect("skill columns always include Icon");
        let expected_center = egui::pos2(
            anchors[icon_index] - widths[icon_index] + SKILL_ICON_SIZE / 2.0,
            rows_rect.top() + SKILL_ROW_HEIGHT / 2.0,
        );

        let mut found_radius = None;
        for &(center, radius) in &circles {
            if (center - expected_center).length() < 0.01 {
                found_radius = Some(radius);
                break;
            }
        }
        let radius = found_radius.unwrap_or_else(|| {
            panic!("no circle painted at the icon slot's center {expected_center:?}: {circles:?}")
        });
        assert_eq!(
            radius, SKILL_ICON_PLACEHOLDER_RADIUS,
            "the painted disc must use SKILL_ICON_PLACEHOLDER_RADIUS, not the full SKILL_ICON_SIZE/2.0 slot"
        );
    }

    /// A stale bound taller than the actual captured image (e.g. the
    /// window shrank between the frame that computed the bound and the
    /// later frame the screenshot reply landed on) must clamp to the
    /// image's real height, never index past it.
    #[test]
    fn screenshot_crop_height_px_clamps_to_the_image_height() {
        assert_eq!(screenshot_crop_height_px(10_000.0, 1.0, 600), 600);
    }

    /// Issue #184: the skill window tracks the main panel because its fills
    /// share the main panel's opaque baseline — same slider value, same
    /// perceived transparency, however different the colors underneath.
    #[test]
    fn skill_window_fills_share_the_panel_opacity_baseline() {
        for (name, fill) in [
            ("chrome", SKILL_CHROME_FILL),
            ("panel", SKILL_PANEL_FILL),
            ("column header", SKILL_COLUMN_HEADER_FILL),
        ] {
            assert_eq!(fill.a(), PANEL_FILL.a(), "{name}");
            let half = fill.gamma_multiply(0.5).a();
            assert_eq!(half, PANEL_FILL.gamma_multiply(0.5).a(), "{name} at 50%");
        }
    }

    // -- reference-measured geometry (issue #200) ---------------------------
    //
    // Every number below is a pixel measurement taken off
    // `docs/reference/shinra-skills-ex.webp` (928x574). That capture is 1:1
    // with WPF DIPs: its header pill measures exactly 34px tall against
    // `Skills.xaml`'s `CornerRadius="17"`, which pins the scale, so these
    // pixel figures port straight across to egui points.

    /// The reference's class icon is `Skills.xaml`'s 50x50, and its header
    /// band measures 70px (content starts at y=2; the selected tab's box
    /// starts at y=71), so the icon fills the padded content area exactly.
    /// Issue #190 settled for 40 only because the header was a too-short 56.
    #[test]
    fn skill_header_icon_exactly_fills_the_padded_header() {
        assert_eq!(SKILL_HEADER_ICON_SIZE, 50.0);
        assert_eq!(
            SKILL_HEADER_HEIGHT - 2.0 * SKILL_HEADER_PAD_Y,
            SKILL_HEADER_ICON_SIZE
        );
    }

    /// The header pill measures 34px tall in the reference — exactly twice
    /// `Skills.xaml`'s `CornerRadius="17"`, i.e. a true stadium. Deriving
    /// the height from the header band instead made it 40 tall against a 17
    /// radius, which is visibly not one.
    #[test]
    fn skill_header_pill_is_a_stadium() {
        assert_eq!(SKILL_PILL_HEIGHT, 2.0 * f32::from(SKILL_PILL_CORNER_RADIUS));
    }

    /// Band heights measured off the reference: header 2..70, tab strip
    /// 71..92, column header 93..132, then a 44px row pitch (ten row text
    /// centers, 156 through 553). The stack still has to leave two rows
    /// visible at the minimum window size.
    #[test]
    fn reference_band_heights_leave_two_rows_at_the_minimum_size() {
        assert_eq!(SKILL_HEADER_HEIGHT, 70.0);
        assert_eq!(SKILL_TAB_HEIGHT, 22.0);
        assert_eq!(SKILL_COLUMN_HEADER_HEIGHT, 40.0);
        assert_eq!(SKILL_ROW_HEIGHT, 44.0);
        let chrome = SKILL_HEADER_HEIGHT + SKILL_TAB_HEIGHT + SKILL_COLUMN_HEADER_HEIGHT;
        assert!(
            chrome + 2.0 * SKILL_ROW_HEIGHT <= SKILL_WINDOW_MIN_SIZE.y,
            "{chrome} of chrome leaves no room for two rows"
        );
        // `SKILL_WINDOW_SIZE`'s documented promise: a newly opened window
        // shows ten rows before the list scrolls, exactly as the reference
        // capture does (rows y=133..573, ten 44px rows).
        let rows = (SKILL_WINDOW_SIZE.y - chrome) / SKILL_ROW_HEIGHT;
        assert!(rows >= 10.0, "initial window opens on only {rows} rows");
    }

    /// The reference's row icons are 38px across (row 1's disc spans x
    /// 32..69, y 136..173) inside a 44px row, and clear the skill name by
    /// ~9px (name text starts at x=78).
    #[test]
    fn skill_row_icon_matches_the_reference_and_clears_the_name() {
        assert_eq!(SKILL_ICON_SIZE, 38.0);
        const { assert!(SKILL_ICON_SIZE < SKILL_ROW_HEIGHT) };
        let gap = skills::SkillColumn::Icon.width() - SKILL_ICON_SIZE;
        assert!(
            (8.0..=12.0).contains(&gap),
            "icon column must keep a reference-sized gap before the name, got {gap}"
        );
    }

    /// Widening the icon column must not push the column set past the
    /// initial window's content width — `column_anchors_from_widths` would
    /// silently shrink every column to fit (the trap issue #192 hit).
    #[test]
    fn skill_columns_fit_the_initial_window_at_their_stated_widths() {
        // Issue #245: every tab, not just `Dps` — each one lays its own
        // column set out in the same window.
        for tab in skills::SKILL_TABS {
            let total: f32 = tab.columns().iter().map(|c| c.width()).sum();
            assert!(
                total <= SKILL_WINDOW_SIZE.x - 2.0 * SKILL_HEADER_PAD_X,
                "{tab:?} columns total {total}"
            );
        }
    }

    /// Issue #245: a tab with no columns at all (every real tab has some
    /// today — issue #267 gave `Buff`, the last holdout, its own column set
    /// — but `skill_column_header_rect` still has to handle the case, since
    /// nothing in its signature rules it out) must not paint a full 40pt
    /// band of `SKILL_COLUMN_HEADER_FILL` with nothing in it — a stray
    /// lighter strip above the explanatory text. No columns gets no band at
    /// all, so the rows area (and the explanation in it) starts straight
    /// under the tab strip.
    #[test]
    fn a_tab_with_no_columns_gets_no_column_header_band() {
        let rect = egui::Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(800.0, 600.0));
        let tabs_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), 75.0),
            egui::vec2(rect.width(), SKILL_TAB_HEIGHT),
        );
        let col_header_rect = skill_column_header_rect(rect, tabs_rect, &[]);
        assert_eq!(col_header_rect.height(), 0.0);
        assert_eq!(
            skill_rows_rect(rect, col_header_rect).top(),
            tabs_rect.bottom()
        );
    }

    /// Issue #248: the skill table used to be laid out right-to-left from
    /// the window's right inset against fixed column widths, so all surplus
    /// width landed in the *left* gutter — measured at 60px at the minimum
    /// width, 68px at the initial one and ~290px at 1074 wide, i.e.
    /// widening the window shoved the whole table right, behind a growing
    /// empty band.
    ///
    /// Issue #245's `skills::column_widths` is what closes that gap, and it
    /// is the reading the main meter window already has: the leftover goes
    /// to `Name` — the one column with no bounded formatter — so the widths
    /// sum to the content width exactly and `column_anchors_from_widths`'
    /// right-to-left walk lands the leading column on the content's left
    /// edge at every size, the same way `draw_rows` starts the main meter's
    /// anchors from `avail.left()`. Checked end to end here, through the
    /// same two calls `draw_skill_window` makes.
    #[test]
    fn skill_columns_stay_left_inset_at_every_window_width() {
        let columns = skills::SkillTab::Dps.columns();
        let layout_at = |window_width: f32| {
            let left = SKILL_HEADER_PAD_X;
            let right = window_width - SKILL_HEADER_PAD_X;
            let widths = skills::column_widths(columns, right - left);
            (
                column_anchors_from_widths(left, right, &widths, 0.0),
                widths,
            )
        };
        let surplus = 300.0;
        let (narrow, narrow_widths) = layout_at(SKILL_WINDOW_MIN_SIZE.x);
        let (wide, wide_widths) = layout_at(SKILL_WINDOW_MIN_SIZE.x + surplus);

        // The leading column's left edge sits one pad in from the window at
        // both sizes: no dead left gutter opens up as the window grows.
        assert_eq!(narrow[0] - narrow_widths[0], SKILL_HEADER_PAD_X);
        assert_eq!(wide[0] - wide_widths[0], SKILL_HEADER_PAD_X);
        // The surplus went to `Name` rather than to a gutter at either end
        // — every other column keeps its stated width, and the trailing
        // column stays pinned one pad in from the window's right edge.
        let name = columns
            .iter()
            .position(|c| *c == skills::SkillColumn::Name)
            .expect("every tab shows the Name column");
        assert_eq!(wide_widths[name] - narrow_widths[name], surplus);
        for (i, column) in columns.iter().enumerate() {
            if i != name {
                assert_eq!(wide_widths[i], column.width(), "{column:?} was resized");
            }
        }
        assert_eq!(
            *wide.last().unwrap(),
            SKILL_WINDOW_MIN_SIZE.x + surplus - SKILL_HEADER_PAD_X
        );
    }

    /// Below the columns' own sum there is no leftover for
    /// `skills::column_widths` to hand `Name`, so the pre-existing
    /// proportional shrink (`column_anchors_from_widths`) still takes the
    /// full inset width — the table spans the window rather than
    /// left-aligning inside a slot it no longer fits.
    #[test]
    fn skill_columns_still_span_the_window_when_it_is_too_narrow() {
        let columns = skills::SkillTab::Dps.columns();
        let window_width = SKILL_WINDOW_MIN_SIZE.x / 2.0;
        let left = SKILL_HEADER_PAD_X;
        let right = window_width - SKILL_HEADER_PAD_X;
        let widths = skills::column_widths(columns, right - left);
        // Nothing to give away, so the shrink is the anchors' job alone.
        for (width, column) in widths.iter().zip(columns) {
            assert_eq!(*width, column.width());
        }
        let anchors = column_anchors_from_widths(left, right, &widths, 0.0);
        assert_eq!(*anchors.last().unwrap(), right);
        // Same graceful degradation `column_anchors_degrade_gracefully_in_
        // a_narrow_window` pins for the main meter: scaled slots, still
        // ordered, still inside the window.
        assert!(anchors[0] >= 0.0, "columns spilled past the left edge");
        for pair in anchors.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    /// Issue #228: dragging the window down to `SKILL_WINDOW_MIN_SIZE`
    /// used to be reachable at a width far narrower than the columns'
    /// combined budget. `column_anchors_from_widths` scales every column's
    /// *slot* down to fit at that point, but the header labels
    /// (`draw_skill_window`'s column-header loop) are painted unclipped at
    /// fixed size — they don't shrink or elide with their slot — so a
    /// too-narrow floor collided them into unreadable text (e.g.
    /// `Damag%Dmg%Max cr…`). The fix keeps the floor itself wide enough
    /// that no column is ever below the width its label needs: the sum of
    /// every `SkillColumn::width` plus the column header row's left/right
    /// `SKILL_HEADER_PAD_X` inset (the same margin
    /// `skill_columns_fit_the_initial_window_at_their_stated_widths`
    /// checks against for the *initial* size). This is the "raise the
    /// floor" fix from the alternatives the issue lists (eliding text or
    /// progressively dropping columns): it is the smallest change that
    /// removes the collision, and it keeps every column's full label
    /// legible at every reachable size instead of introducing a second,
    /// narrower text-rendering mode.
    #[test]
    fn skill_window_min_width_fits_every_column_at_its_stated_width() {
        // Issue #245: the widest tab, not just `Dps` — every tab's header
        // row is painted unclipped at fixed size, so all of them have to
        // clear the floor.
        let total: f32 = skills::SKILL_TABS
            .iter()
            .map(|tab| tab.columns().iter().map(|c| c.width()).sum::<f32>())
            .fold(0.0_f32, f32::max);
        assert!(
            SKILL_WINDOW_MIN_SIZE.x >= total + 2.0 * SKILL_HEADER_PAD_X,
            "min width {} is narrower than the columns' {total} + padding, \
             so a resize down to it collides their header text",
            SKILL_WINDOW_MIN_SIZE.x
        );
    }

    /// Measured off the reference at x=860, where the game background
    /// behind the window is continuous across all three band edges:
    /// header/tabs (29,28,33), rows (45,44,49), column header (51,50,55).
    /// Three distinct levels, brightest at the column header — the rows are
    /// *not* on the window's chrome fill.
    #[test]
    fn skill_bands_step_up_from_chrome_to_the_column_header() {
        assert!(SKILL_PANEL_FILL.r() > SKILL_CHROME_FILL.r());
        assert!(SKILL_COLUMN_HEADER_FILL.r() > SKILL_PANEL_FILL.r());
    }

    /// In the reference the tab strip's background *is* the window fill:
    /// at y=80 the pixels under the unselected tabs (x=700) match the
    /// header band exactly, while only the selected `Dps` tab (x 2..51)
    /// carries the lighter `#212127` box.
    #[test]
    fn selected_tab_fill_hugs_its_label_instead_of_the_whole_strip() {
        let tabs =
            egui::Rect::from_min_size(egui::pos2(10.0, 60.0), egui::vec2(760.0, SKILL_TAB_HEIGHT));
        let rects = skill_tab_rects(tabs, &[26.0, 40.0]);
        let fill = rects[0];
        assert_eq!(fill.left(), tabs.left());
        assert_eq!(fill.top(), tabs.top());
        assert_eq!(fill.height(), tabs.height());
        assert_eq!(fill.width(), 26.0 + 2.0 * SKILL_HEADER_PAD_X);
        assert!(
            fill.right() < tabs.right(),
            "the rest of the strip must stay window fill"
        );
        // Issue #245: the next tab starts exactly where this one ends —
        // the strip is a row of abutting boxes, not one filled band.
        assert_eq!(rects[1].left(), fill.right());
        assert_eq!(rects[1].width(), 40.0 + 2.0 * SKILL_HEADER_PAD_X);
    }

    /// Issue #245: every tab's box is sized to its own label, so a click
    /// lands on the tab whose text is under the pointer.
    #[test]
    fn tab_rects_tile_the_strip_left_to_right_without_gaps() {
        let tabs =
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(760.0, SKILL_TAB_HEIGHT));
        let rects = skill_tab_rects(tabs, &[10.0, 20.0, 30.0]);
        assert_eq!(rects.len(), 3);
        for pair in rects.windows(2) {
            assert_eq!(pair[0].right(), pair[1].left());
        }
        assert!(rects.last().expect("three tabs").right() <= tabs.right());
    }

    /// Issue #245: each tab keeps its own sort, so switching away and back
    /// returns to the ordering the user chose rather than resetting.
    #[test]
    fn every_tab_starts_on_its_own_default_sort_and_keeps_it() {
        let mut tabs = SkillTabs::default();
        assert_eq!(tabs.selected, skills::SkillTab::Dps);
        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Damage);

        tabs.selected = skills::SkillTab::Heal;
        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Heal);
        tabs.sort_mut().toggle(skills::SkillColumn::Hits);

        tabs.selected = skills::SkillTab::Dps;
        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Damage);
        tabs.selected = skills::SkillTab::Heal;
        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Hits);
    }

    /// Issue #245: an untracked tab explains itself instead of implying
    /// the fight simply had none of that. Issue #267 gave `Buff` a real
    /// decode path, so it now falls through to the ordinary per-tab
    /// "nothing yet" wording like every other tracked tab, rather than
    /// `untracked_message` (which is `None` for it today).
    #[test]
    fn a_tracked_tab_with_no_rows_says_nothing_recorded_rather_than_untracked() {
        assert_eq!(skills::SkillTab::Buff.untracked_message(), None);
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, skills::SkillTab::Buff, 0),
            Some("Nothing recorded yet")
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, skills::SkillTab::Heal, 0),
            Some("No healing recorded yet")
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, skills::SkillTab::Heal, 3),
            None
        );
    }

    /// Pins the column-header band's rect (issue #200): flush with the
    /// window edges, directly beneath the tab strip, `SKILL_COLUMN_HEADER_
    /// HEIGHT` tall — this is the rect `SKILL_COLUMN_HEADER_FILL` paints.
    #[test]
    fn column_header_rect_sits_flush_beneath_the_tab_strip() {
        let rect = egui::Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(800.0, 600.0));
        let tabs_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), 75.0),
            egui::vec2(rect.width(), SKILL_TAB_HEIGHT),
        );
        let col_header_rect =
            skill_column_header_rect(rect, tabs_rect, skills::SkillTab::Dps.columns());
        assert_eq!(col_header_rect.left(), rect.left());
        assert_eq!(col_header_rect.right(), rect.right());
        assert_eq!(col_header_rect.top(), tabs_rect.bottom());
        assert_eq!(col_header_rect.height(), SKILL_COLUMN_HEADER_HEIGHT);
    }

    /// Pins the scrollable row-list band's rect (issue #200): everything
    /// below the column header down to the window's bottom edge — this is
    /// the rect `SKILL_PANEL_FILL` paints, distinct from the chrome fill
    /// above it.
    #[test]
    fn rows_rect_fills_everything_below_the_column_header() {
        let rect = egui::Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(800.0, 600.0));
        let col_header_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), 132.0),
            egui::vec2(rect.width(), SKILL_COLUMN_HEADER_HEIGHT),
        );
        let rows_rect = skill_rows_rect(rect, col_header_rect);
        assert_eq!(rows_rect.left(), rect.left());
        assert_eq!(rows_rect.right(), rect.right());
        assert_eq!(rows_rect.top(), col_header_rect.bottom());
        assert_eq!(rows_rect.bottom(), rect.bottom());
    }

    /// Issue #218: the drag surface used to be `ui.max_rect()` — the whole
    /// viewport, edges included — so the eight `resize_zones` strips could
    /// never be grabbed. The band now stops short of them on all three
    /// sides it touches, exactly as the main header's does.
    #[test]
    fn drag_band_is_the_header_inset_by_the_resize_edge() {
        let rect = skill_window_rect();
        let header = skill_header_rect(rect);
        let band = skill_drag_band(header);
        assert_eq!(band.top(), header.top() + RESIZE_EDGE);
        assert_eq!(band.left(), header.left() + RESIZE_EDGE);
        assert_eq!(band.right(), header.right() - RESIZE_EDGE);
        assert_eq!(band.bottom(), header.bottom());
    }

    /// The same inset, stated as the property that actually matters: every
    /// edge resize strip keeps a live pixel the drag band does not cover.
    #[test]
    fn drag_band_leaves_every_edge_resize_zone_reachable() {
        let rect = skill_window_rect();
        let band = skill_drag_band(skill_header_rect(rect));
        let zones = resize_zones(rect);
        let (north, south, west, east) = (zones[0].0, zones[1].0, zones[2].0, zones[3].0);
        assert!(band.top() >= north.bottom(), "north strip is covered");
        assert!(band.left() >= west.right(), "west strip is covered");
        assert!(band.right() <= east.left(), "east strip is covered");
        assert!(band.bottom() <= south.top(), "south strip is covered");
    }

    /// Issue #218's scroll bug: a drag sense over the row list wedges
    /// egui's `dragged_id()`, which gates *all* wheel scrolling
    /// (`scroll_area.rs`' `is_hovering_outer_rect`). The band must not
    /// reach the rows band at all.
    #[test]
    fn drag_band_never_covers_the_row_list() {
        let rect = skill_window_rect();
        let header = skill_header_rect(rect);
        let tabs = egui::Rect::from_min_size(
            egui::pos2(rect.left(), header.bottom()),
            egui::vec2(rect.width(), SKILL_TAB_HEIGHT),
        );
        let rows = skill_rows_rect(
            rect,
            skill_column_header_rect(rect, tabs, skills::SkillTab::Dps.columns()),
        );
        assert!(!skill_drag_band(header).intersects(rows));
    }

    /// Issue #218: the close glyph was a bare 20pt square. The reference
    /// (`Skills.xaml:214-224`) is a 16pt `Svg.Close` with an 8pt margin per
    /// side — a 32pt target, big enough to hit and big enough for a
    /// circular hover wash of radius `SKILL_CLOSE_HIT_SIZE / 2`.
    #[test]
    fn close_button_is_a_32pt_target_around_a_16pt_glyph() {
        assert_eq!(SKILL_CLOSE_HIT_SIZE, SKILL_CLOSE_GLYPH_SIZE + 2.0 * 8.0);
        // The wash is `#1fff` — white at alpha 0x11 — written premultiplied
        // only because `from_white_alpha` is not `const`.
        assert_eq!(
            SKILL_CLOSE_HOVER_FILL,
            egui::Color32::from_white_alpha(0x11)
        );
        let rect = skill_window_rect();
        let close = skill_close_rect(rect);
        assert_eq!(close.width(), SKILL_CLOSE_HIT_SIZE);
        assert_eq!(close.height(), SKILL_CLOSE_HIT_SIZE);
        assert_eq!(close.right(), rect.right() - SKILL_HEADER_PAD_X);
        assert_eq!(close.top(), rect.top() + SKILL_HEADER_PAD_Y);
    }

    /// Issue #246: the Deaths pill sits `SKILL_DEATHS_PILL_GAP` after the
    /// player name's right edge, not right-aligned against the close
    /// button — the reference (`Skills.xaml:172-176`) places its whole pill
    /// cluster in the header's flexible middle column, right after the
    /// name. That preferred placement only applies while the cluster still
    /// clears the close button — a typical name (well short of the
    /// window's width) does.
    #[test]
    fn deaths_pill_sits_one_reference_gap_after_a_typical_name() {
        let rect = skill_window_rect();
        let header = skill_header_rect(rect);
        let close_left = skill_close_rect(rect).left();
        let cluster_width = 64.0;
        let name_right = header.left() + 200.0;

        let left = skill_deaths_pill_left(name_right, close_left, cluster_width);

        assert_eq!(left, name_right + SKILL_DEATHS_PILL_GAP);
        assert!(
            left + cluster_width < close_left,
            "a typical name plus a typical cluster width must clear the close button"
        );
    }

    /// `PlayerRow::name` is an unbounded, network-decoded string
    /// (`crates/meter/src/stats.rs`), so a long enough name would push the
    /// gap-after-name placement's cluster past the close button outright.
    /// Uses a name wide enough that the unclamped formula would have landed
    /// the cluster's *left* edge past the close button's left edge, to
    /// prove this is more than a last-pixel clamp.
    #[test]
    fn deaths_pill_stays_left_of_the_close_button_for_a_long_name() {
        let rect = skill_window_rect();
        let close_rect = skill_close_rect(rect);
        let cluster_width = 64.0;
        let name_right = close_rect.left() + 500.0;

        let left = skill_deaths_pill_left(name_right, close_rect.left(), cluster_width);

        // The unclamped gap-after-name formula would have placed the
        // cluster well past the close button; the clamp must have
        // overridden it.
        assert!(left < name_right + SKILL_DEATHS_PILL_GAP);
        assert!(
            left + cluster_width <= close_rect.left() - SKILL_HEADER_PAD_X,
            "the cluster must stay clear of the close button no matter how wide the name is"
        );
        assert!(
            left + cluster_width <= rect.right(),
            "the cluster must also stay inside the window"
        );
    }

    /// Issue #254: the death-time pill follows the Deaths pill by one
    /// reference gap, and it is the *cluster* — not its first pill — that
    /// keeps one header pad clear of the close button.
    #[test]
    fn the_death_time_pill_follows_the_deaths_pill_by_one_gap() {
        let rect = skill_window_rect();
        let close_left = skill_close_rect(rect).left();
        let deaths_width = 64.0;
        let death_time_width = 96.0;

        let cluster = skill_header_pill_cluster_width(deaths_width, Some(death_time_width));
        assert_eq!(
            cluster,
            deaths_width + SKILL_HEADER_PILL_GAP + death_time_width
        );

        // A name that already reaches the close button, so it is
        // `skill_deaths_pill_left`'s clamp — not issue #246's
        // after-the-name preference — placing the cluster here: the clamp
        // is the branch that owes the close button its clearance.
        let deaths_left = skill_deaths_pill_left(close_left, close_left, cluster);
        let death_time_left = deaths_left + deaths_width + SKILL_HEADER_PILL_GAP;
        assert!(
            death_time_left >= deaths_left + deaths_width,
            "the two pills must not overlap"
        );
        assert_eq!(
            death_time_left + death_time_width + SKILL_HEADER_PAD_X,
            close_left,
            "the last pill in the cluster is the one that clears the close button"
        );
    }

    /// A row with no death-time pill lays out exactly as it did before
    /// issue #254 — the gap is part of the pill, not of the Deaths pill.
    #[test]
    fn a_cluster_with_one_pill_is_just_that_pill() {
        assert_eq!(skill_header_pill_cluster_width(64.0, None), 64.0);
    }

    /// Issue #270 follow-up: the header used to paint the player name with
    /// no clip, max-width or elision at all, so a long enough name ran
    /// underneath the pill cluster — and #254's death-time pill widened the
    /// cluster, cutting the safe name length further. Renders a real frame
    /// (through the real font, not a hardcoded pixel budget) with both
    /// pills present — the worst-case cluster width — at the window's
    /// narrowest allowed size, and asserts the painted name never reaches
    /// past one `SKILL_HEADER_PAD_X` short of the cluster.
    #[test]
    fn an_overlong_name_stays_clear_of_the_pill_cluster() {
        let row = PlayerRow {
            // Issue #246 moved the cluster to just after the name, so a
            // merely long name no longer runs under it — the cluster
            // follows. Only a name long enough to hit
            // `skill_deaths_pill_left`'s clamp can still collide, so this
            // is one.
            name: "A Name So Long It Would Otherwise Run Under The Header Pills"
                .repeat(3)
                .to_string(),
            deaths: 3,
            dead_ms: Some(12_400),
            ..sample_row(None)
        };
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_MIN_SIZE);
        let mut tabs_state = SkillTabs::default();

        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_skill_window(
                    ui,
                    &row,
                    &mut tabs_state,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
                    &mut WindowGesture::default(),
                );
            },
        );

        let mut name_rect: Option<egui::Rect> = None;
        for clipped in &output.shapes {
            collect_name_text_boxes(&clipped.shape, clipped.clip_rect, &row.name, &mut name_rect);
        }
        output.drop_without_applying_deltas();

        let name_rect = name_rect.expect("the header never painted the player name");
        let deaths_pill = StatPill {
            value: "3",
            icon: None,
            icon_side: COUNTER_GLYPH_SIDE,
            size: FONT_SIZE_PILL_VALUE,
            value_color: egui::Color32::WHITE,
            icon_color: COUNTER_ICON_COLOR,
            icon_first: true,
            corner_radius: egui::CornerRadius::same(SKILL_PILL_CORNER_RADIUS),
            fill: SKILL_PANEL_FILL,
            stroke: None,
        };
        let death_time_text = skill_death_time_text(row.dead_ms).unwrap();
        let painter = egui::Painter::new(ctx.clone(), egui::LayerId::debug(), screen_rect);
        let deaths_width = pill_size(
            pill_text_size(&painter, &deaths_pill),
            deaths_pill.icon_side,
            SKILL_PILL_HEIGHT,
        )
        .x;
        let death_time_pill = StatPill {
            value: &death_time_text,
            ..deaths_pill
        };
        let death_time_width = pill_size(
            pill_text_size(&painter, &death_time_pill),
            death_time_pill.icon_side,
            SKILL_PILL_HEIGHT,
        )
        .x;
        // The name runs far past the space before the cluster, so issue
        // #246's preference gives way to `skill_deaths_pill_left`'s clamp
        // and the cluster sits one header pad clear of the close button —
        // exactly where it sat before that preference existed.
        let cluster_left = skill_close_rect(screen_rect).left()
            - SKILL_HEADER_PAD_X
            - skill_header_pill_cluster_width(deaths_width, Some(death_time_width));

        // Equality, not just "clear of": the clip is what stops the name,
        // so its painted right edge lands *on* the cluster's pad. A name
        // that stopped short of it would mean the clamp never engaged and
        // this test had lost its teeth.
        assert!(
            (name_rect.right() - (cluster_left - SKILL_HEADER_PAD_X)).abs() <= 0.5,
            "name right edge {} must be clipped exactly one SKILL_HEADER_PAD_X \
             short of the cluster (cluster_left {cluster_left}, pad {SKILL_HEADER_PAD_X})",
            name_rect.right()
        );
    }

    /// Issue #254: the total is an estimate (the revive edge is inferred
    /// from the player's next action), so it wears a `~` — except at zero,
    /// which is exact, and except for a history row, which has no measured
    /// total at all and gets no pill rather than a misleading `00:00`.
    #[test]
    fn death_time_text_marks_the_estimate_but_not_an_exact_zero() {
        assert_eq!(skill_death_time_text(None), None);
        assert_eq!(skill_death_time_text(Some(0)).as_deref(), Some("00:00"));
        assert_eq!(
            skill_death_time_text(Some(12_400)).as_deref(),
            Some("~00:12")
        );
        assert_eq!(
            skill_death_time_text(Some(159_000)).as_deref(),
            Some("~02:39")
        );
        assert_eq!(
            skill_death_time_text(Some(120 * 60 * 1000)).as_deref(),
            Some("~120:00"),
            "minutes keep counting up rather than rolling over, like fmt_duration"
        );
    }

    /// Issue #218 (follow-up): `U+2715` came out as tofu — an empty box —
    /// because the bold family's font chain does not cover it. The cross is
    /// vector art now, exactly as the reference draws it
    /// (`<Path Data="{StaticResource Svg.Close}" ...>`), so no font chain
    /// can regress it.
    #[test]
    fn close_cross_is_two_centred_diagonals_the_size_of_the_glyph_box() {
        let close = skill_close_rect(skill_window_rect());
        let [[a0, a1], [b0, b1]] = skill_close_cross(close);

        let box_rect = egui::Rect::from_points(&[a0, a1, b0, b1]);
        assert_eq!(box_rect.center(), close.center());
        assert_eq!(box_rect.width(), SKILL_CLOSE_GLYPH_SIZE);
        assert_eq!(box_rect.height(), SKILL_CLOSE_GLYPH_SIZE);
        assert!(
            close.contains_rect(box_rect),
            "the cross must fit its target"
        );

        // Opposite diagonals, not two parallel strokes.
        assert!((a1.x - a0.x) > 0.0 && (a1.y - a0.y) > 0.0);
        assert!((b1.x - b0.x) < 0.0 && (b1.y - b0.y) > 0.0);

        // And every endpoint stays inside the circular hover wash.
        for point in [a0, a1, b0, b1] {
            assert!(point.distance(close.center()) <= SKILL_CLOSE_HIT_SIZE / 2.0);
        }
    }

    /// Issue #218: the close button had no hover feedback at all, so
    /// nothing about it read as clickable. The wash is a circle of
    /// `SKILL_CLOSE_HOVER_FILL` filling the 32pt target, painted only while
    /// the pointer is over it — the same shape of check
    /// `toggle_button_suppresses_its_hover_fill_while_a_screenshot_capture_
    /// is_in_flight` makes for the toolbar's buttons, and the unhovered
    /// half is the sanity check that the wash is genuinely conditional
    /// rather than always painted.
    #[test]
    fn the_close_button_paints_its_hover_wash_only_while_hovered() {
        let row = PlayerRow {
            skills: vec![sample_skill_row(1550)],
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            ..sample_row(None)
        };
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_MIN_SIZE);
        let mut tabs = SkillTabs::default();

        // Two frames per probe: egui resolves hover against the widgets the
        // *previous* frame registered, so a single frame would report the
        // close button unhovered however the pointer is placed.
        let mut fills_with_pointer_at = |pointer: egui::Pos2| -> Vec<egui::Color32> {
            let mut fills = Vec::new();
            for frame in 0..2 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen_rect),
                        events: vec![egui::Event::PointerMoved(pointer)],
                        ..Default::default()
                    },
                    |ui| {
                        draw_skill_window(
                            ui,
                            &row,
                            &mut tabs,
                            SkillWindowSource::Live,
                            &icons,
                            Opacity::OPAQUE,
                            &mut WindowGesture::default(),
                        );
                    },
                );
                if frame == 1 {
                    for clipped in &output.shapes {
                        collect_circle_fills(&clipped.shape, &mut fills);
                    }
                }
                output.drop_without_applying_deltas();
            }
            fills
        };

        let hovered = fills_with_pointer_at(skill_close_rect(screen_rect).center());
        assert!(
            hovered.contains(&SKILL_CLOSE_HOVER_FILL),
            "hovering the close button must paint its wash: {hovered:?}"
        );

        // The player-name end of the header: inside the window, nowhere
        // near the close button.
        let elsewhere = fills_with_pointer_at(skill_header_rect(screen_rect).left_center());
        assert!(
            !elsewhere.contains(&SKILL_CLOSE_HOVER_FILL),
            "the wash must not paint with the pointer elsewhere: {elsewhere:?}"
        );
    }

    /// Issue #218 (follow-up): with the list overflowing, the reference
    /// shows a persistent thin thumb down the right edge of the rows band.
    /// `ScrollStyle::solid()` alone did not put one on screen.
    #[test]
    fn an_overflowing_row_list_paints_a_scrollbar_in_its_rows_band() {
        let row = PlayerRow {
            skills: (0..40).map(|i| sample_skill_row(1550 + i)).collect(),
            ..sample_row(None)
        };
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        // Deliberately at the window's floor: 40 rows cannot fit, so a bar
        // is needed.
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_MIN_SIZE);
        let mut tabs = SkillTabs::default();
        // Two frames: egui animates a scroll bar in, so the first frame's
        // `show_factor` is still 0 and paints nothing either way.
        for _ in 0..2 {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    ..Default::default()
                },
                |ui| {
                    draw_skill_window(
                        ui,
                        &row,
                        &mut tabs,
                        SkillWindowSource::Live,
                        &icons,
                        Opacity::OPAQUE,
                        &mut WindowGesture::default(),
                    );
                },
            )
            .drop_without_applying_deltas();
        }
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_skill_window(
                    ui,
                    &row,
                    &mut tabs,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
                    &mut WindowGesture::default(),
                );
            },
        );
        let mut rects = Vec::new();
        for clipped in &output.shapes {
            painted_rects(&clipped.shape, &mut rects);
        }
        output.drop_without_applying_deltas();

        let header = skill_header_rect(screen_rect);
        let tabs = egui::Rect::from_min_size(
            egui::pos2(screen_rect.left(), header.bottom()),
            egui::vec2(screen_rect.width(), SKILL_TAB_HEIGHT),
        );
        let rows = skill_rows_rect(
            screen_rect,
            skill_column_header_rect(screen_rect, tabs, skills::SkillTab::Dps.columns()),
        );
        let bar = rects.iter().find(|r| {
            r.right() == rows.right()
                && r.width() == SKILL_SCROLL_BAR_WIDTH
                && r.top() >= rows.top()
                && r.bottom() <= rows.bottom()
                && r.height() >= SKILL_SCROLL_THUMB_MIN_HEIGHT
        });
        assert!(
            bar.is_some(),
            "no scroll thumb painted down the rows band's right edge"
        );
    }

    /// A list that fits needs no thumb at all — the reference shows one
    /// only where there is something to scroll.
    #[test]
    fn no_scroll_thumb_when_the_list_fits() {
        let rows = egui::Rect::from_min_size(egui::pos2(0.0, 132.0), egui::vec2(360.0, 88.0));
        assert_eq!(skill_scroll_thumb(rows, 88.0, 0.0), None);
        assert_eq!(skill_scroll_thumb(rows, 40.0, 0.0), None);
    }

    /// The thumb rides the gutter: fixed width at the band's right edge,
    /// proportional height with a floor, and it travels from the band's top
    /// at offset 0 to its bottom at the end of the scroll (issue #218).
    #[test]
    fn scroll_thumb_tracks_the_offset_inside_the_rows_band() {
        let rows = egui::Rect::from_min_size(egui::pos2(0.0, 132.0), egui::vec2(360.0, 88.0));
        let content = 264.0;

        let top = skill_scroll_thumb(rows, content, 0.0).expect("the list overflows");
        assert_eq!(top.width(), SKILL_SCROLL_BAR_WIDTH);
        assert_eq!(top.right(), rows.right());
        assert_eq!(top.top(), rows.top());
        // A third of the list is on screen, so the thumb is a third as tall.
        assert!((top.height() - rows.height() / 3.0).abs() < 0.001);

        let bottom =
            skill_scroll_thumb(rows, content, content - rows.height()).expect("the list overflows");
        assert!((bottom.bottom() - rows.bottom()).abs() < 0.001);
        assert_eq!(bottom.height(), top.height());

        // Overscroll (egui's elastic bounce) must not push it out of the band.
        let past = skill_scroll_thumb(rows, content, 10_000.0).expect("the list overflows");
        assert!((past.bottom() - rows.bottom()).abs() < 0.001);

        // A very long list still leaves something grabbable.
        let tiny = skill_scroll_thumb(rows, 100_000.0, 0.0).expect("the list overflows");
        assert_eq!(tiny.height(), SKILL_SCROLL_THUMB_MIN_HEIGHT);
    }

    /// The rows lay out inside the gutter, so nothing they paint can cover
    /// the thumb (issue #218).
    #[test]
    fn rows_content_reserves_the_thumbs_gutter() {
        let rows = egui::Rect::from_min_size(egui::pos2(0.0, 132.0), egui::vec2(360.0, 88.0));
        let content = skill_rows_content_rect(rows);
        assert_eq!(content.right(), rows.right() - SKILL_SCROLL_BAR_WIDTH);
        assert_eq!(content.left(), rows.left());
        assert_eq!(content.height(), rows.height());
        let thumb = skill_scroll_thumb(rows, 264.0, 0.0).expect("the list overflows");
        assert!(!content.intersects(thumb) || content.right() <= thumb.left());
    }

    /// Issue #299: the Buff tab is the one tab whose rows are *always*
    /// empty (`SkillTab::rows` hands it `&[]` unconditionally, since
    /// buff tracking isn't implemented yet — see `skills::SkillTab::Buff`),
    /// so `draw_skill_window` always takes its "nothing recorded" early
    /// return while it is selected — before `drive_window_gesture` ever
    /// runs. A move/resize gesture begun while the window happened to be
    /// showing Buff was therefore never driven to completion: not moved,
    /// not ended when the pointer let go, just silently frozen mid-drag
    /// with its whole stale `start_pointer`/`start_rect` delta waiting to
    /// be applied in one jump the moment the user switched back to a tab
    /// with rows — reading as the window (and the fight beneath it)
    /// suddenly resetting. Switching tabs must never affect anything but
    /// which breakdown is displayed.
    #[test]
    fn switching_to_the_buff_tab_still_lets_an_in_flight_gesture_end() {
        let row = sample_row(None);
        let mut tabs = SkillTabs {
            selected: skills::SkillTab::Buff,
            ..Default::default()
        };

        let mut gesture = WindowGesture::default();
        gesture.begin(GestureKind::Move, egui::pos2(10.0, 10.0), window_rect());
        assert_eq!(gesture.kind(), Some(GestureKind::Move));

        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_SIZE);
        // No pointer input this frame: the drag has already been
        // released, and `drive_window_gesture` is the only thing that
        // ever notices that and ends the gesture.
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_skill_window(
                    ui,
                    &row,
                    &mut tabs,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
                    &mut gesture,
                );
            },
        )
        .drop_without_applying_deltas();

        assert_eq!(
            gesture.kind(),
            None,
            "the Buff tab's empty state must not skip driving an in-flight gesture to completion"
        );
    }

    #[test]
    fn a_second_right_click_does_not_close_an_open_breakdown() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        // A second open, at a different would-be placement, must be a
        // no-op: the first placement is what stays, and the uid is never
        // removed — re-right-clicking an open row re-shows it, it never
        // toggles it closed (D1).
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(99.0, 99.0))
        });
        assert!(windows.contains_key(&1));
        assert_eq!(windows[&1].pos, egui::pos2(1.0, 1.0));
    }

    /// The map is deliberately untouched for an already-open uid, so the
    /// return value is the only thing that can tell the caller to raise and
    /// focus the buried viewport (issue #189).
    #[test]
    fn a_second_right_click_reports_the_uid_as_already_open() {
        let mut windows = std::collections::BTreeMap::new();

        let first = open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        let second = open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });

        assert!(!first, "the first right-click is what opens the window");
        assert!(second, "the second must ask the caller to focus it");
    }

    /// The round trip issue #181 is about: a size the user resized to is
    /// tracked off the live viewport, and is still what the next
    /// `ViewportBuilder` reads after the window has been reopened — the
    /// resize is no longer discarded in favour of `SKILL_WINDOW_SIZE`.
    #[test]
    fn a_resized_breakdown_reopens_at_the_size_it_was_left_at() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        let resized = egui::vec2(900.0, 640.0);

        track_skill_window_size(
            &mut windows.get_mut(&1).unwrap().size,
            skill_inner_rect_of(resized),
        );
        // A later right-click on the same row must not undo it: `state`
        // never runs for an already-open uid.
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });

        assert_eq!(windows[&1].size, resized);
    }

    /// Fractional-DPI jitter is not a resize — it would otherwise be fed
    /// back into the builder and resize the window at itself every repaint.
    #[test]
    fn track_skill_window_size_ignores_sub_pixel_jitter() {
        let mut size = SKILL_WINDOW_SIZE;

        track_skill_window_size(
            &mut size,
            skill_inner_rect_of(SKILL_WINDOW_SIZE + egui::vec2(0.25, 0.25)),
        );

        assert_eq!(size, SKILL_WINDOW_SIZE);
    }

    /// Same plausibility floor `track_window_size` applies to the root
    /// window: a zeroed size must never become the size it reopens at.
    #[test]
    fn track_skill_window_size_ignores_an_absurd_zeroed_size() {
        let mut size = SKILL_WINDOW_SIZE;

        track_skill_window_size(&mut size, skill_inner_rect_of(egui::Vec2::ZERO));

        assert_eq!(size, SKILL_WINDOW_SIZE);
    }

    #[test]
    fn closing_a_breakdown_drops_its_uid() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        close_skill_window(&mut windows, 1);
        assert!(!windows.contains_key(&1));
    }

    #[test]
    fn a_uid_missing_from_the_snapshot_is_skipped_not_closed() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        open_skill_window(&mut windows, 2, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(2.0, 2.0))
        });
        let rows = vec![PlayerRow {
            uid: 1,
            ..sample_row(None)
        }];

        let to_draw = skill_windows_to_draw(&windows, &rows, None);

        assert_eq!(to_draw.len(), 1, "only the live uid is drawn this frame");
        assert_eq!(to_draw[0].1, 1);
        assert!(
            windows.contains_key(&2),
            "a uid missing from the snapshot must stay open, not be closed"
        );
    }

    /// PR #221 review: opening History must not repaint an already-open
    /// live breakdown with the historical fight's numbers.
    #[test]
    fn a_live_window_keeps_its_live_row_while_a_historical_fight_is_open() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        let live = vec![skill_source_row(1, 1_000)];
        let historical = vec![skill_source_row(1, 9_999)];

        let to_draw = skill_windows_to_draw(&windows, &live, Some((7, &historical)));

        assert_eq!(to_draw.len(), 1);
        assert_eq!(
            to_draw[0].0.damage, 1_000,
            "a window opened from Live must keep reading the live encounter, \
             whatever the history view is showing"
        );
    }

    /// The mirror image: a window opened from a past fight ignores the live
    /// encounter's row for that uid, and stops drawing (without closing)
    /// once its own fight is no longer the open one.
    #[test]
    fn a_historical_window_only_draws_for_its_own_fight() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::History(7), || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });
        let live = vec![skill_source_row(1, 1_000)];
        let historical = vec![skill_source_row(1, 9_999)];

        let its_own = skill_windows_to_draw(&windows, &live, Some((7, &historical)));
        let another = skill_windows_to_draw(&windows, &live, Some((8, &historical)));
        let back_on_live = skill_windows_to_draw(&windows, &live, None);

        assert_eq!(its_own.len(), 1);
        assert_eq!(its_own[0].0.damage, 9_999);
        assert!(another.is_empty(), "a different fight is not this window's");
        assert!(
            back_on_live.is_empty(),
            "the live encounter's row for the same uid is not this window's either"
        );
        assert!(
            windows.contains_key(&1),
            "a window with no rows to draw this frame stays open"
        );
    }

    /// Right-clicking a historical row for a player whose live window is
    /// already open retargets that window onto the fight the gesture came
    /// from — the placement the user dragged it to survives.
    #[test]
    fn a_right_click_from_history_retargets_an_open_live_window() {
        let mut windows = std::collections::BTreeMap::new();
        open_skill_window(&mut windows, 1, SkillWindowSource::Live, || {
            skill_window_state(egui::pos2(1.0, 1.0))
        });

        let already_open =
            open_skill_window(&mut windows, 1, SkillWindowSource::History(7), || {
                skill_window_state(egui::pos2(99.0, 99.0))
            });

        assert!(already_open);
        assert_eq!(windows[&1].source, SkillWindowSource::History(7));
        assert_eq!(windows[&1].pos, egui::pos2(1.0, 1.0));
    }

    #[test]
    fn skill_window_rows_come_from_the_window_s_own_source() {
        let live = vec![skill_source_row(1, 1_000)];
        let historical = vec![skill_source_row(1, 9_999)];
        // `PlayerRow` is not `PartialEq`, so the rows are told apart by the
        // one field `skill_source_row` varies.
        let damage = |rows: Option<&[PlayerRow]>| rows.map(|rows| rows[0].damage);

        assert_eq!(
            damage(skill_window_rows(
                SkillWindowSource::Live,
                &live,
                Some((7, &historical))
            )),
            Some(1_000)
        );
        assert_eq!(
            damage(skill_window_rows(
                SkillWindowSource::History(7),
                &live,
                Some((7, &historical))
            )),
            Some(9_999)
        );
        assert_eq!(
            damage(skill_window_rows(
                SkillWindowSource::History(7),
                &live,
                None
            )),
            None,
            "a historical window whose fight is no longer open has nothing to draw"
        );
    }

    /// A zero-skill row means two different things (PR #221 review): a live
    /// row is a roster-preloaded or not-yet-hitting player mid-fight, a
    /// historical one is a fight saved before schema v2 stored per-skill
    /// totals (issue #222) — settled either way, not still arriving.
    #[test]
    fn skill_window_empty_message_is_worded_for_the_window_s_source() {
        let dps = skills::SkillTab::Dps;
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::History(7), dps, 0),
            Some("No per-skill data recorded for this fight")
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, dps, 0),
            Some("No damage recorded yet"),
            "an ongoing fight's rows are still coming — claiming nothing was \
             recorded for it would be wrong"
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, dps, 3),
            None
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::History(7), dps, 3),
            None
        );
    }

    #[test]
    fn clicking_a_column_header_toggles_its_sort() {
        let row = PlayerRow {
            skills: vec![sample_skill_row(1550), sample_skill_row(1551)],
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            ..sample_row(None)
        };
        let mut tabs = SkillTabs::default();
        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Damage);

        // "Skill name" is the `Name` header's plain (unselected) label —
        // the default sort is `Damage`, so this is never the active-sort
        // text `header_label` would instead paint.
        click_skill_window_at(&row, &mut tabs, "Skill name");

        assert_eq!(tabs.sort_mut().column, skills::SkillColumn::Name);
        assert!(
            tabs.sort_mut().descending,
            "a newly-clicked column always starts descending (D9)"
        );
    }

    #[test]
    fn clicking_the_close_glyph_closes_the_window() {
        let row = PlayerRow {
            skills: vec![sample_skill_row(1550)],
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            ..sample_row(None)
        };
        let mut tabs = SkillTabs::default();

        // The close button is aimed at geometrically: it paints two line
        // segments now, not a glyph (issue #218).
        let closed = click_skill_window(&row, &mut tabs, |_| {
            skill_close_rect(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                SKILL_WINDOW_SIZE,
            ))
            .center()
        });

        assert!(
            closed,
            "clicking the close glyph must report the window closed (D2)"
        );
    }
}
