//! Pure view-model logic for the per-player skill breakdown window (issue
//! #16): column definitions/formatting, sort state, skill-name resolution,
//! and window placement. Deliberately free of `egui::Ui`/`Context` — plain
//! geometry types (`egui::Rect`/`Pos2`/`Vec2`) are fine as inputs/outputs,
//! same split `ui.rs` already uses for `column_anchors`/`row_bar_frac` —
//! so all of it is unit-testable with no window. `crates/app/src/ui.rs`
//! (T4) owns painting this; it must not be touched here.

use crate::ui::{fmt_pct0, fmt_short};
use bpsr_meter::SkillRow;

/// One column of the Dps tab (issue #16, D5), in on-screen order.
///
/// `Icon` is the reference's leading skill-icon column (issue #192). It was
/// omitted originally because this repo had no skill-icon assets; it now
/// vendors them under `crates/app/assets/skills/`, keyed by
/// `bpsr_meter::tables::skill_icon`. It is the one column here that paints
/// no text and is not sortable (`sortable`) — the reference's expander
/// chevron that sits left of it stays omitted, since this window has no
/// expand/collapse tier to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillColumn {
    Icon,
    Name,
    Damage,
    DmgPct,
    CritPct,
    MaxCrit,
    AvgCrit,
    AvgWhite,
    Avg,
    Hits,
    Crits,
    HitPerMin,
}

impl SkillColumn {
    /// Column header text, matching the reference's labels verbatim (D5).
    pub fn label(self) -> &'static str {
        match self {
            // The icon column is unlabelled in the reference, and there is
            // nothing to abbreviate a picture to in 48pt anyway.
            SkillColumn::Icon => "",
            SkillColumn::Name => "Skill name",
            SkillColumn::Damage => "Damage",
            SkillColumn::DmgPct => "% Dmg",
            SkillColumn::CritPct => "% Crit",
            SkillColumn::MaxCrit => "Max crit",
            SkillColumn::AvgCrit => "Avg crit",
            SkillColumn::AvgWhite => "Avg white",
            SkillColumn::Avg => "Avg",
            SkillColumn::Hits => "Hits",
            SkillColumn::Crits => "Crits",
            SkillColumn::HitPerMin => "Hit/m",
        }
    }

    /// Fixed on-screen width this column reserves, in points. Sized the
    /// same way `ColumnKind::spec`'s widths in `settings.rs` are: enough
    /// for the widest text the column's formatter can plausibly produce,
    /// rounded up to the next multiple of 8. `Name` gets a generous flex
    /// budget since skill names (unlike every other column here) are not
    /// bounded by `fmt_short`.
    pub fn width(self) -> f32 {
        match self {
            // The 38pt icon (`SKILL_ICON_SIZE` in `ui.rs`) plus the 10pt
            // gap that separates it from the skill name — issue #200
            // measured the reference's row icon at 38px across, clearing
            // the name text (which starts at x=78) by ~9px.
            SkillColumn::Icon => 48.0,
            SkillColumn::Name => 160.0,
            // `fmt_short` bounds every damage/count figure to ~7 chars,
            // same budget `ColumnKind::Damage`/`Hits` use in settings.rs.
            SkillColumn::Damage
            | SkillColumn::MaxCrit
            | SkillColumn::AvgCrit
            | SkillColumn::AvgWhite
            | SkillColumn::Avg
            | SkillColumn::Hits
            | SkillColumn::Crits => 56.0,
            // Whole-number percentage, same budget as `ColumnKind::CritPct`.
            SkillColumn::DmgPct | SkillColumn::CritPct => 40.0,
            // `format!("{:.2}", ..)` on a hits-per-minute rate; a few
            // hundred hits/min is already an extreme value, so 5-6 chars
            // plus the 2-decimal tail comfortably fits.
            SkillColumn::HitPerMin => 48.0,
        }
    }

    /// Renders this column's value for one row. Built only on the existing
    /// `fmt_short`/`fmt_pct0` formatters (reused from `ui.rs`, not
    /// reinvented) plus one 2-decimal `format!` for `Hit/m`, the reference's
    /// only 2-decimal column (D5).
    pub fn text(self, row: &SkillRow) -> String {
        match self {
            // Painted as a texture, not text — see `SkillColumn`'s doc
            // comment and the row-paint loop in `ui.rs`.
            SkillColumn::Icon => String::new(),
            SkillColumn::Name => skill_display_name(row.skill_id),
            SkillColumn::Damage => fmt_short(row.damage),
            SkillColumn::DmgPct => fmt_pct0(row.share_pct),
            SkillColumn::CritPct => fmt_pct0(row.crit_pct),
            SkillColumn::MaxCrit => fmt_short(row.max_crit),
            SkillColumn::AvgCrit => fmt_short(row.avg_crit as i64),
            SkillColumn::AvgWhite => fmt_short(row.avg_white as i64),
            SkillColumn::Avg => fmt_short(row.avg as i64),
            SkillColumn::Hits => fmt_short(row.hits as i64),
            SkillColumn::Crits => fmt_short(row.crit_hits as i64),
            SkillColumn::HitPerMin => format!("{:.2}", row.hits_per_min),
        }
    }

    /// Whether clicking this column's header sorts by it. Every column but
    /// `Icon` does; ordering rows by which picture they carry is
    /// meaningless, and the reference's icon column is not clickable
    /// either. `ui.rs`'s header loop skips the toggle for an unsortable
    /// column, and `sort_rows` returns early on one, so no code path can
    /// reach `numeric_key`'s `unreachable!` for it.
    pub fn sortable(self) -> bool {
        !matches!(self, SkillColumn::Icon)
    }

    /// This column's numeric sort key, as `f64` so every column but `Name`
    /// (which sorts lexically on its display name, not here) shares one
    /// comparison path in `sort_rows`. Every source field already fits an
    /// `f64` without meaningful precision loss at realistic damage scales.
    fn numeric_key(self, row: &SkillRow) -> f64 {
        match self {
            SkillColumn::Icon | SkillColumn::Name => {
                unreachable!("{self:?} does not sort numerically; see `sortable`")
            }
            SkillColumn::Damage => row.damage as f64,
            SkillColumn::DmgPct => row.share_pct as f64,
            SkillColumn::CritPct => row.crit_pct as f64,
            SkillColumn::MaxCrit => row.max_crit as f64,
            SkillColumn::AvgCrit => row.avg_crit,
            SkillColumn::AvgWhite => row.avg_white,
            SkillColumn::Avg => row.avg,
            SkillColumn::Hits => row.hits as f64,
            SkillColumn::Crits => row.crit_hits as f64,
            SkillColumn::HitPerMin => row.hits_per_min,
        }
    }
}

/// Resolves a raw skill id to display text, via the generated `skill_name`
/// table (issue #16 / T2). Mirrors the existing `Monster #<id>` / `Player
/// {uid}` placeholder idiom (D13) for an id the table doesn't know, and for
/// a negative id (`skill_id` is `i32`; the table is keyed `u32`) — both fall
/// through the same `unwrap_or_else` rather than needing a separate branch.
pub fn skill_display_name(skill_id: i32) -> String {
    u32::try_from(skill_id)
        .ok()
        .and_then(bpsr_meter::tables::skill_name)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Skill #{skill_id}"))
}

/// Resolves a raw skill id to its vendored icon basename (issue #192), via
/// the generated `skill_icon` table. `None` — an id the table names no icon
/// for, or a negative id, both falling through the same `try_from` — is the
/// normal "no icon" answer, and the caller paints a generated placeholder
/// (issue #275) in its place. Note that `Some` is not a promise the icon is
/// *shipped*: the table names every icon BPSR-ZDPS references, and
/// `SkillIcons::get` returns `None` for any whose PNG is not committed
/// under `crates/app/assets/skills/`.
pub fn skill_icon_basename(skill_id: i32) -> Option<&'static str> {
    u32::try_from(skill_id)
        .ok()
        .and_then(bpsr_meter::tables::skill_icon)
}

/// Derives a 1-2 character monogram from a resolved skill display name
/// (issue #275's placeholder for the 65 observed ids with no upstream
/// icon). Splits `name` on every non-alphanumeric boundary, so punctuation
/// ("Lucky Strike (Battle Axe)"), dashes ("Wild Wolf - Coordinated
/// Attack") and the `skill_display_name` fallback's `#` ("Skill #2426")
/// all separate into real words rather than leaking a symbol into the
/// glyph. A multi-word name takes the first letter of its first two words
/// ("Falcon Strike" -> "FS", "Skill #2426" -> "S2"); a single-word name
/// takes its own first two characters ("Burn" -> "BU"). Every character of
/// the result goes through `char::to_uppercase` — Unicode-aware casing,
/// not an ASCII-only flip — so a non-Latin name still produces glyphs in
/// its own script rather than being mangled or dropped; a script with no
/// case distinction (e.g. CJK) passes its characters through unchanged,
/// which is exactly what a monogram of it should do.
///
/// Returns `None` only when `name` has no alphanumeric content at all
/// (empty, or pure punctuation/whitespace) — there is nothing to derive a
/// monogram from, so the caller keeps painting the flat `SKILL_ICON_EMPTY`
/// disc for that one case. In practice no observed skill id reaches this:
/// `skill_display_name` always falls back to `Skill #<id>`, which is never
/// blank.
pub fn skill_monogram(name: &str) -> Option<String> {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let chars: Vec<char> = match words.as_slice() {
        [] => return None,
        [only] => only.chars().take(2).collect(),
        [first, second, ..] => {
            vec![
                first.chars().next().unwrap(),
                second.chars().next().unwrap(),
            ]
        }
    };
    let monogram: String = chars.into_iter().flat_map(char::to_uppercase).collect();
    Some(monogram)
}

/// Sort state for one open breakdown window. Per-window, not global (D9) —
/// each `SkillSort` lives on that window's own entry in `OverlayApp`'s open
/// set (T4), never shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillSort {
    pub column: SkillColumn,
    pub descending: bool,
}

impl Default for SkillSort {
    /// Damage descending (D9), matching the order `SkillRow` already
    /// arrives in from `Meter::snapshot` — so the default sort is a no-op
    /// on first paint.
    fn default() -> Self {
        Self {
            column: SkillColumn::Damage,
            descending: true,
        }
    }
}

impl SkillSort {
    /// Applies a header click (D9): the same column flips direction; a
    /// different column takes over sorted descending, matching the
    /// reference's "always starts descending on a new column" behavior.
    pub fn toggle(&mut self, clicked: SkillColumn) {
        if self.column == clicked {
            self.descending = !self.descending;
        } else {
            self.column = clicked;
            self.descending = true;
        }
    }

    /// This column's header text, with a direction arrow appended only when
    /// it is the active sort column (D9) — every other header stays plain.
    pub fn header_label(&self, column: SkillColumn) -> String {
        if column == self.column {
            let arrow = if self.descending {
                " \u{2193}"
            } else {
                " \u{2191}"
            };
            format!("{}{arrow}", column.label())
        } else {
            column.label().to_string()
        }
    }
}

/// Sorts `rows` in place per `sort` (stable, so equal keys keep their
/// incoming relative order — the meter's damage-descending order is a
/// reasonable tiebreaker for every other column). `Name` sorts on the
/// resolved display name (what the user actually reads); every other
/// sortable column sorts on its own numeric field via
/// `SkillColumn::numeric_key`. `Icon` is not sortable and is a no-op here.
pub fn sort_rows(rows: &mut [SkillRow], sort: SkillSort) {
    // An unsortable column (`Icon`) leaves the incoming meter order alone.
    // `ui.rs` never toggles the sort onto one, so this is belt-and-braces
    // against a future caller constructing such a `SkillSort` by hand — but
    // it is what keeps `numeric_key`'s `unreachable!` genuinely unreachable.
    if !sort.column.sortable() {
        return;
    }
    if sort.column == SkillColumn::Name {
        rows.sort_by(|a, b| {
            let ord = skill_display_name(a.skill_id).cmp(&skill_display_name(b.skill_id));
            if sort.descending { ord.reverse() } else { ord }
        });
        return;
    }
    rows.sort_by(|a, b| {
        let ord = sort
            .column
            .numeric_key(a)
            .partial_cmp(&sort.column.numeric_key(b))
            .unwrap_or(std::cmp::Ordering::Equal);
        if sort.descending { ord.reverse() } else { ord }
    });
}

/// Computes the breakdown window's top-left position (D2/D3), following the
/// reference's exact fallback chain (`Skills.xaml.cs:31-71`): dock
/// immediately right of the main window; if the window wouldn't fit there,
/// dock immediately left; if it fits neither side, centre it on the
/// monitor. `monitor == None` means no monitor geometry is knowable (e.g. a
/// platform query failed) — dock right unconditionally, since nothing
/// better can be computed.
pub fn place_window(
    main_outer: egui::Rect,
    monitor: Option<egui::Vec2>,
    size: egui::Vec2,
) -> egui::Pos2 {
    let top = main_outer.top();
    let right_x = main_outer.right();
    let Some(monitor_size) = monitor else {
        return egui::Pos2::new(right_x, top);
    };

    if right_x + size.x <= monitor_size.x {
        return egui::Pos2::new(right_x, top);
    }

    let left_x = main_outer.left() - size.x;
    if left_x >= 0.0 {
        return egui::Pos2::new(left_x, top);
    }

    egui::Pos2::new(
        (monitor_size.x - size.x) / 2.0,
        (monitor_size.y - size.y) / 2.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_row(skill_id: i32, damage: i64) -> SkillRow {
        SkillRow {
            skill_id,
            damage,
            share_pct: 0.0,
            crit_pct: 0.0,
            max_crit: 0,
            avg_crit: 0.0,
            avg_white: 0.0,
            avg: 0.0,
            hits: 0,
            crit_hits: 0,
            hits_per_min: 0.0,
        }
    }

    #[test]
    fn default_sort_is_damage_descending() {
        let sort = SkillSort::default();
        assert_eq!(sort.column, SkillColumn::Damage);
        assert!(sort.descending);
    }

    #[test]
    fn clicking_the_same_column_flips_direction() {
        let mut sort = SkillSort::default();
        sort.toggle(SkillColumn::Damage);
        assert_eq!(sort.column, SkillColumn::Damage);
        assert!(!sort.descending);
        sort.toggle(SkillColumn::Damage);
        assert!(sort.descending);
    }

    #[test]
    fn clicking_a_different_column_resets_to_descending() {
        let mut sort = SkillSort {
            column: SkillColumn::Damage,
            descending: false,
        };
        sort.toggle(SkillColumn::Hits);
        assert_eq!(sort.column, SkillColumn::Hits);
        assert!(sort.descending);
    }

    #[test]
    fn the_active_header_carries_a_direction_arrow() {
        let sort = SkillSort::default();
        assert_eq!(sort.header_label(SkillColumn::Damage), "Damage \u{2193}");
        assert_eq!(sort.header_label(SkillColumn::Hits), "Hits");
    }

    #[test]
    fn the_icon_column_is_the_only_unsortable_one() {
        assert!(!SkillColumn::Icon.sortable());
        for column in [
            SkillColumn::Name,
            SkillColumn::Damage,
            SkillColumn::DmgPct,
            SkillColumn::CritPct,
            SkillColumn::MaxCrit,
            SkillColumn::AvgCrit,
            SkillColumn::AvgWhite,
            SkillColumn::Avg,
            SkillColumn::Hits,
            SkillColumn::Crits,
            SkillColumn::HitPerMin,
        ] {
            assert!(column.sortable(), "{column:?} should be sortable");
        }
    }

    #[test]
    fn sorting_by_the_icon_column_is_a_no_op_rather_than_a_panic() {
        let mut rows = vec![skill_row(1, 100), skill_row(2, 300)];
        sort_rows(
            &mut rows,
            SkillSort {
                column: SkillColumn::Icon,
                descending: true,
            },
        );
        let ids: Vec<i32> = rows.iter().map(|r| r.skill_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn an_unmapped_skill_id_has_no_icon_basename() {
        assert_eq!(skill_icon_basename(2_000_000_000), None);
        assert_eq!(skill_icon_basename(-1), None);
    }

    #[test]
    fn a_known_skill_id_resolves_to_its_vendored_icon_basename() {
        // 1201 ("Raincall Surge - Stage 1") is the first entry in
        // `crates/meter/data/SkillOverridesIcons.json`.
        assert_eq!(skill_icon_basename(1201), Some("weapon_mz-01_skill_atk"));
    }

    #[test]
    fn an_unmapped_skill_id_falls_back_to_a_placeholder() {
        // 2_000_000_000 sits well outside the generated table's coverage.
        assert_eq!(skill_display_name(2_000_000_000), "Skill #2000000000");
    }

    #[test]
    fn a_negative_skill_id_falls_back_to_a_placeholder() {
        assert_eq!(skill_display_name(-1), "Skill #-1");
    }

    #[test]
    fn sort_rows_orders_rows_by_the_active_column_descending() {
        let mut rows = vec![skill_row(1, 100), skill_row(2, 300), skill_row(3, 200)];
        sort_rows(&mut rows, SkillSort::default());
        let ids: Vec<i32> = rows.iter().map(|r| r.skill_id).collect();
        assert_eq!(ids, vec![2, 3, 1]);
    }

    #[test]
    fn sort_rows_orders_ascending_when_direction_is_flipped() {
        let mut rows = vec![skill_row(1, 100), skill_row(2, 300), skill_row(3, 200)];
        sort_rows(
            &mut rows,
            SkillSort {
                column: SkillColumn::Damage,
                descending: false,
            },
        );
        let ids: Vec<i32> = rows.iter().map(|r| r.skill_id).collect();
        assert_eq!(ids, vec![1, 3, 2]);
    }

    #[test]
    fn the_window_docks_right_when_it_fits() {
        let main_outer =
            egui::Rect::from_min_size(egui::Pos2::new(100.0, 50.0), egui::Vec2::new(400.0, 600.0));
        let pos = place_window(
            main_outer,
            Some(egui::Vec2::new(1920.0, 1080.0)),
            egui::Vec2::new(300.0, 500.0),
        );
        assert_eq!(pos, egui::Pos2::new(500.0, 50.0));
    }

    #[test]
    fn the_window_docks_left_when_it_does_not_fit_on_the_right() {
        let main_outer =
            egui::Rect::from_min_size(egui::Pos2::new(1700.0, 50.0), egui::Vec2::new(200.0, 600.0));
        let pos = place_window(
            main_outer,
            Some(egui::Vec2::new(1920.0, 1080.0)),
            egui::Vec2::new(300.0, 500.0),
        );
        // main_outer.left() = 1700, right() = 1900; right + 300 = 2200 > 1920 (doesn't fit right)
        // left - 300 = 1400 >= 0 (fits left)
        assert_eq!(pos, egui::Pos2::new(1400.0, 50.0));
    }

    #[test]
    fn the_window_centres_when_it_fits_on_neither_side() {
        let main_outer =
            egui::Rect::from_min_size(egui::Pos2::new(0.0, 50.0), egui::Vec2::new(1900.0, 600.0));
        let pos = place_window(
            main_outer,
            Some(egui::Vec2::new(1920.0, 1080.0)),
            egui::Vec2::new(300.0, 500.0),
        );
        // right() = 1900; right + 300 = 2200 > 1920 (doesn't fit right)
        // left - 300 = -300 < 0 (doesn't fit left)
        assert_eq!(
            pos,
            egui::Pos2::new((1920.0 - 300.0) / 2.0, (1080.0 - 500.0) / 2.0)
        );
    }

    #[test]
    fn with_no_monitor_information_the_window_docks_right_unconditionally() {
        let main_outer =
            egui::Rect::from_min_size(egui::Pos2::new(1700.0, 50.0), egui::Vec2::new(200.0, 600.0));
        let pos = place_window(main_outer, None, egui::Vec2::new(300.0, 500.0));
        assert_eq!(pos, egui::Pos2::new(1900.0, 50.0));
    }

    // == Issue #275: skill-icon monogram placeholder ======================

    #[test]
    fn a_single_word_name_takes_its_own_first_two_characters() {
        assert_eq!(skill_monogram("Burn").as_deref(), Some("BU"));
    }

    #[test]
    fn a_two_word_name_takes_the_first_letter_of_each_word() {
        assert_eq!(skill_monogram("Falcon Strike").as_deref(), Some("FS"));
    }

    #[test]
    fn a_punctuated_multi_word_name_splits_on_the_punctuation_not_through_it() {
        // Real #275 example: parentheses must not leak into the glyph, and
        // the two leading real words ("Lucky", "Strike") win over the
        // parenthesized weapon-variant qualifier.
        assert_eq!(
            skill_monogram("Lucky Strike (Battle Axe)").as_deref(),
            Some("LS")
        );
        // A dash-separated name (issue example) splits the same way.
        assert_eq!(
            skill_monogram("Wild Wolf - Coordinated Attack").as_deref(),
            Some("WW")
        );
    }

    #[test]
    fn the_number_id_fallback_name_still_yields_a_monogram() {
        // `skill_display_name`'s own fallback for an id with no table entry
        // ("Skill #2426") must not choke on the `#` or the digits.
        assert_eq!(skill_monogram("Skill #2426").as_deref(), Some("S2"));
    }

    #[test]
    fn a_name_with_no_alphanumeric_content_yields_no_monogram() {
        // Empty, whitespace-only, and pure-punctuation names all have
        // nothing to derive a glyph from — `ui.rs` keeps painting the flat
        // `SKILL_ICON_EMPTY` disc for exactly this case.
        assert_eq!(skill_monogram(""), None);
        assert_eq!(skill_monogram("   "), None);
        assert_eq!(skill_monogram("---"), None);
    }

    #[test]
    fn a_non_latin_name_uppercases_per_script_instead_of_mangling() {
        // Cyrillic has case, so it uppercases like Latin does.
        assert_eq!(skill_monogram("привет мир").as_deref(), Some("ПМ"));
        // CJK has no case distinction; the characters pass through as-is
        // rather than being dropped or replaced.
        assert_eq!(skill_monogram("火焰 斩击").as_deref(), Some("火斩"));
    }

    #[test]
    fn monogram_derivation_is_deterministic() {
        for name in ["Burn", "Falcon Strike", "Lucky Strike (Battle Axe)"] {
            assert_eq!(skill_monogram(name), skill_monogram(name));
        }
    }

    #[test]
    fn every_skill_id_issue_275_cites_as_art_less_resolves_a_real_monogram() {
        // The concrete ids issue #275's evidence table names (its top
        // uncovered-damage rows) — not an exhaustive list of all 65, which
        // the issue itself only characterizes in aggregate, but the
        // ground-truth cases it gives skill ids and names for. Every one of
        // these must resolve a name (down to the `Skill #<id>` fallback,
        // never blank) and, from that name, a real placeholder glyph —
        // never falling through to the blank `SKILL_ICON_EMPTY` disc.
        let art_less_ids = [
            2031103, // Lucky Strike (Battle Axe) — 17.12% of damage
            2203291, // Falcon Strike
            2031105, // Lucky Strike (Forest Ring)
            1700820, // Wild Wolf - Coordinated Attack
            35107,   // Formless Flame Slash - Stage 1
            2031102, // Lucky Strike (Staff)
            35108,   // Formless Flame Slash - Stage 2
            2031101, // Lucky Strike (Tachi)
            35109,   // Formless Flame Slash - Stage 3
            2203521, // Implosion (Steel Beak Trigger)
            2208172, // Burn
        ];
        for id in art_less_ids {
            let name = skill_display_name(id);
            let monogram = skill_monogram(&name);
            assert!(
                monogram.is_some(),
                "id {id} (name {name:?}) should still produce a placeholder monogram"
            );
        }
    }
}
