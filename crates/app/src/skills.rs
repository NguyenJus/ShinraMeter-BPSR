//! Pure view-model logic for the per-player skill breakdown window (issue
//! #16): column definitions/formatting, sort state, skill-name resolution,
//! and window placement. Deliberately free of `egui::Ui`/`Context` — plain
//! geometry types (`egui::Rect`/`Pos2`/`Vec2`) are fine as inputs/outputs,
//! same split `ui/table.rs` already uses for `column_anchors`/`row_bar_frac` —
//! so all of it is unit-testable with no window. `crates/app/src/ui/skill_window.rs`
//! (T4) owns painting this; it must not be touched here.

use crate::ui::{fmt_duration, fmt_pct0, fmt_short};
use bpsr_meter::{PlayerRow, SkillRow};

/// One column of a breakdown tab (issue #16, D5; issue #245), in
/// on-screen order. Which of these a tab shows is [`SkillTab::columns`] —
/// this enum is the union across every tab, so a column that means the same
/// thing on two tabs (`Name`, `CritPct`, `Hits`, ...) is one variant used
/// twice rather than two near-duplicates.
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
    /// The Heal tab's amount column and its share (issue #245). Distinct
    /// variants from `Damage`/`DmgPct` purely for their labels — the
    /// underlying `SkillRow` field is the same, because `SkillRow` is
    /// reused verbatim for every tab (see `bpsr_meter::PlayerRow::heals`).
    Heal,
    HealPct,
    /// The Skill dealt / Skill received tabs' amount column and its share
    /// (issue #245). Those two tabs mix damage and healing under one skill
    /// id, so neither "Damage" nor "Heal" is an honest label for the
    /// figure; "Amount" is what the reference's own dealt/received log
    /// calls it (`SkillLog.xaml`'s `SkillAmount`).
    Amount,
    AmountPct,
    /// The Skill casts tab (issue #245), modelled on the reference's
    /// `SkillsHeaderCounter` (skill name + a single count column). Reads
    /// `SkillRow::hits`/`hits_per_min`, which for a cast breakdown are use
    /// counts rather than landed hits.
    Casts,
    CastPerMin,
    /// The Buff tab's uptime-percentage column (issue #267). Reads
    /// `SkillRow::share_pct`, repurposed the same way `HealPct`/`AmountPct`
    /// already repurpose it — see `bpsr_meter::PlayerRow::buffs`'s doc
    /// comment for the full field remapping.
    UptimePct,
    /// The Buff tab's apply-count column (issue #267). Reads
    /// `SkillRow::hits`, repurposed from "hits landed" to "apply→remove
    /// cycles closed" — a distinct label from `Casts`/`Hits` because
    /// neither word fits what this counts.
    BuffCount,
    /// The Buff tab's mean-duration-per-application column (issue #267).
    /// Reads `SkillRow::avg` (milliseconds) through `fmt_duration`, the
    /// same `MM:SS` formatter the header stat pills use, rather than
    /// `fmt_short`'s raw-number formatting every other `Avg`-shaped column
    /// uses — a duration is a time, not a count.
    Duration,
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
            SkillColumn::Heal => "Heal",
            SkillColumn::HealPct => "% Heal",
            SkillColumn::Amount => "Amount",
            SkillColumn::AmountPct => "% Amt",
            SkillColumn::Casts => "Casts",
            SkillColumn::CastPerMin => "Cast/m",
            SkillColumn::UptimePct => "% Uptime",
            SkillColumn::BuffCount => "Count",
            SkillColumn::Duration => "Duration",
        }
    }

    /// Fixed on-screen width this column reserves, in points. Sized the
    /// same way `ColumnKind::spec`'s widths in `settings.rs` are: enough
    /// for the widest text the column can plausibly paint, rounded up to
    /// the next multiple of 8. "Widest text" is the wider of two things —
    /// the column's own formatted values, and its *header label*.
    ///
    /// Issue #245's live-window pass found the header half had never been
    /// measured: every width here was eyeballed off `fmt_short`'s longest
    /// *value* alone, while `draw_skill_window` paints header labels
    /// right-aligned on each column's anchor at fixed size, so a label
    /// wider than its slot spills *left* over its neighbour. Four of the
    /// six tabs' header rows were overlapping at both the initial and the
    /// minimum width: Dps and Heal read `Avg criAvg white`, Skill dealt
    /// and Skill received read `Amount ↓% Amt` with no gap at all.
    /// Sorting made it worse, because the sorted column's label carries
    /// `SkillSort::header_label`'s trailing arrow on top of its own text —
    /// Heal sorted by `% Crit` degraded to `Hea% Hea% Crit ↑`.
    ///
    /// So every width below clears its own widest label *including that
    /// arrow*, and `ui/skill_window.rs`'s
    /// `every_tab_header_label_fits_its_column_at_every_sort_state`
    /// measures that through the exact font the header loop paints with,
    /// for every tab and every sort state — a relabelled or re-budgeted
    /// column can't quietly reintroduce the overlap.
    ///
    /// `Name` gets a generous flex budget since skill names (unlike every
    /// other column here) are not bounded by `fmt_short`.
    pub fn width(self) -> f32 {
        match self {
            // The 38pt icon (`SKILL_ICON_SIZE` in `ui/skill_window.rs`) plus the 10pt
            // gap that separates it from the skill name — issue #200
            // measured the reference's row icon at 38px across, clearing
            // the name text (which starts at x=78) by ~9px.
            SkillColumn::Icon => 48.0,
            // Issue #248: the old 160.0 was a flat, undocumented guess —
            // narrow enough that "Harmonious Fire Avalanche" ("Skill name"
            // column, row 6 of `shinra-skills-ex.webp` — a real skill name
            // from the reference's own capture, not something this
            // project's `skill_name` table happens to carry, since the
            // reference is a different build/decoder of the same game)
            // measures 156.9pt through this app's own `regular
            // (FONT_SIZE_ROW)` font (`ui/skill_window.rs`'s `skill_name_column_clears_
            // the_widest_real_skill_name_before_the_next_column` test),
            // leaving under 4pt of trailing room before the `Damage`
            // column starts.
            //
            // The reference itself does not pin an exact trailing gap here:
            // `Skills.xaml`'s row template has no fixed `Name`-column width
            // for us to read off, and the capture's own gap after a long
            // name is inflated by a per-skill "loadout set" watermark
            // (e.g. "Balder's Seal") this app doesn't render. Absent a
            // pinned number, this reuses `ui/skill_window.rs`'s `SKILL_HEADER_PAD_X`
            // (12.0) doubled — 24.0 — as the trailing gap: it is the same
            // "breathing room" unit the header already uses twice over
            // (icon-to-name, and now name-to-pill via `SKILL_DEATHS_PILL_
            // GAP`'s derivation), and it comfortably clears the reference's
            // *tightest* measured inter-column header gaps (16-18px, e.g.
            // "Avg crit" -> "Avg white"). 156.9 + 24.0 = 180.9, rounded up
            // to the next multiple of 8 = 184.0.
            SkillColumn::Name => 184.0,
            // `fmt_short` bounds every damage/count figure to ~7 chars,
            // same budget `ColumnKind::Damage`/`Hits` use in settings.rs —
            // and these five keep it, their labels being the short ones:
            // sorted, "Avg" measures 38.2pt, "Hits" 39.0, "Crits" 42.8,
            // "Heal" 42.4 and "Casts" 47.0, all inside 56.
            SkillColumn::Avg
            | SkillColumn::Hits
            | SkillColumn::Crits
            | SkillColumn::Heal
            | SkillColumn::Casts => 56.0,
            // Same 7-char value budget, but the header label is the wider
            // half here. Sorted (arrow included), "Damage" measures
            // 64.5pt, "Amount" 63.3, "Max crit" 62.6, "Avg crit" 60.2 and
            // "Avg white" 73.9 — and "Avg white" overflows 56 even
            // unsorted, which is the pair that collided on Dps and Heal.
            SkillColumn::Damage | SkillColumn::Amount => 72.0,
            SkillColumn::MaxCrit | SkillColumn::AvgCrit => 64.0,
            SkillColumn::AvgWhite => 80.0,
            // A whole-number percentage fits `ColumnKind::CritPct`'s 40pt
            // with room to spare; the labels do not — "% Dmg" is already
            // 41.6pt bare and 57.9 sorted, "% Heal" 56.2 sorted, against
            // "% Amt"'s 54.7 and "% Crit"'s 51.0.
            SkillColumn::DmgPct | SkillColumn::HealPct => 64.0,
            SkillColumn::CritPct | SkillColumn::AmountPct => 56.0,
            // `format!("{:.2}", ..)` on a hits-per-minute rate; a few
            // hundred hits/min is already an extreme value, so 5-6 chars
            // plus the 2-decimal tail fit well inside these — which are
            // set by the sorted "Hit/m" (49.3pt) and "Cast/m" (57.3)
            // labels instead.
            SkillColumn::HitPerMin => 56.0,
            SkillColumn::CastPerMin => 64.0,
            // Issue #267, measured through `every_tab_header_label_fits_
            // its_column_at_every_sort_state` (`ui/skill_window.rs`) the same way every
            // other width here is: sorted (arrow included), "% Uptime"
            // measures 72.97pt and "Duration" 67.41pt, both wider than the
            // `DmgPct`/`HealPct`/`Avg`-family budgets their own values
            // would otherwise fit. `Count`'s value and label both fit the
            // plain `Hits`/`Casts` 56pt budget.
            SkillColumn::UptimePct => 80.0,
            SkillColumn::Duration => 72.0,
            SkillColumn::BuffCount => 56.0,
        }
    }

    /// Renders this column's value for one row. Built only on the existing
    /// `fmt_short`/`fmt_pct0` formatters (reused from `ui/skill_window.rs`, not
    /// reinvented) plus one 2-decimal `format!` for `Hit/m`, the reference's
    /// only 2-decimal column (D5).
    pub fn text(self, row: &SkillRow) -> String {
        match self {
            // Painted as a texture, not text — see `SkillColumn`'s doc
            // comment and the row-paint loop in `ui/skill_window.rs`.
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
            SkillColumn::Heal | SkillColumn::Amount => fmt_short(row.damage),
            SkillColumn::HealPct | SkillColumn::AmountPct => fmt_pct0(row.share_pct),
            SkillColumn::Casts => fmt_short(row.hits as i64),
            SkillColumn::CastPerMin => format!("{:.2}", row.hits_per_min),
            SkillColumn::UptimePct => fmt_pct0(row.share_pct),
            SkillColumn::BuffCount => fmt_short(row.hits as i64),
            SkillColumn::Duration => fmt_duration(row.avg.max(0.0) as u64),
        }
    }

    /// Whether clicking this column's header sorts by it. Every column but
    /// `Icon` does; ordering rows by which picture they carry is
    /// meaningless, and the reference's icon column is not clickable
    /// either. `ui/skill_window.rs`'s header loop skips the toggle for an unsortable
    /// column, and `sort_rows` returns early on one, so no code path can
    /// reach `numeric_key`'s `unreachable!` for it.
    pub fn sortable(self) -> bool {
        !matches!(self, SkillColumn::Icon)
    }

    /// Whether this column's text hugs the left edge of its cell rather
    /// than the right. Only `Name` does — every figure column is
    /// right-aligned so its digits line up down the list, exactly as in
    /// the reference. `Icon` paints no text at all and its answer is
    /// unused.
    pub fn left_aligned(self) -> bool {
        matches!(self, SkillColumn::Name)
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
            SkillColumn::Heal | SkillColumn::Amount => row.damage as f64,
            SkillColumn::HealPct | SkillColumn::AmountPct => row.share_pct as f64,
            SkillColumn::Casts => row.hits as f64,
            SkillColumn::CastPerMin => row.hits_per_min,
            SkillColumn::UptimePct => row.share_pct as f64,
            SkillColumn::BuffCount => row.hits as f64,
            SkillColumn::Duration => row.avg,
        }
    }
}

/// One tab of the breakdown window (issue #245), in tab-strip order.
///
/// The reference (`Skills.xaml:227-236`) offers seven: Dps, Heal, Mana,
/// Buff, Counter, SkillDealt, SkillReceived. `Mana` is dropped — BPSR has
/// no mana resource in the decoded packet stream at all, so it could only
/// ever be an empty tab about nothing. `Counter` is renamed `Casts`, which
/// is what its single count column actually measures
/// (`SkillsHeaderCounter.xaml.cs:13`) and what issue #245 asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillTab {
    Dps,
    Heal,
    Buff,
    Casts,
    Dealt,
    Received,
}

/// Every tab, in the order the strip paints them — the reference's own
/// order with `Mana` removed.
pub const SKILL_TABS: [SkillTab; 6] = [
    SkillTab::Dps,
    SkillTab::Heal,
    SkillTab::Buff,
    SkillTab::Casts,
    SkillTab::Dealt,
    SkillTab::Received,
];

/// The Dps tab's columns (issue #16, D5) — the reference's
/// `SkillsHeaderDps` plus issue #192's leading icon.
const DPS_COLUMNS: [SkillColumn; 12] = [
    SkillColumn::Icon,
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
];

/// The Heal tab's columns (issue #245), from the reference's
/// `SkillsHeaderHeal.xaml.cs`: skill name, heal, avg crit, avg white, avg,
/// max crit, max white, % crit, hits, crits.
///
/// `Max white` is the one reference column dropped: `bpsr_meter::SkillRow`
/// keeps a running max of *crit* values only (`SkillStats::max_crit`), and
/// adding a second running max would change the row contract the on-disk
/// fight history serialises. Every other column is a straight port.
const HEAL_COLUMNS: [SkillColumn; 11] = [
    SkillColumn::Icon,
    SkillColumn::Name,
    SkillColumn::Heal,
    SkillColumn::HealPct,
    SkillColumn::CritPct,
    SkillColumn::MaxCrit,
    SkillColumn::AvgCrit,
    SkillColumn::AvgWhite,
    SkillColumn::Avg,
    SkillColumn::Hits,
    SkillColumn::Crits,
];

/// The Skill dealt / Skill received tabs' columns (issue #245). Both tabs
/// answer the same question in opposite directions, so they share one
/// column set: what landed, how much of it, how hard it hit.
const AMOUNT_COLUMNS: [SkillColumn; 10] = [
    SkillColumn::Icon,
    SkillColumn::Name,
    SkillColumn::Amount,
    SkillColumn::AmountPct,
    SkillColumn::CritPct,
    SkillColumn::MaxCrit,
    SkillColumn::Avg,
    SkillColumn::Hits,
    SkillColumn::Crits,
    SkillColumn::HitPerMin,
];

/// The Skill casts tab's columns (issue #245) — the reference's
/// `SkillsHeaderCounter` (name + one count), plus the per-minute rate the
/// Dps tab already establishes, since a cast count with no denominator is
/// hard to read across fights of different lengths.
const CAST_COLUMNS: [SkillColumn; 4] = [
    SkillColumn::Icon,
    SkillColumn::Name,
    SkillColumn::Casts,
    SkillColumn::CastPerMin,
];

/// The Buff tab's columns (issue #267) — the reference's
/// `EnduranceDebuffHeader`: name, uptime %, count, duration. No leading
/// `Icon`: there is no vendored buff-icon table (only skills have one), so
/// an icon column here would paint nothing but the generated placeholder
/// disc for every single row.
const BUFF_COLUMNS: [SkillColumn; 4] = [
    SkillColumn::Name,
    SkillColumn::UptimePct,
    SkillColumn::BuffCount,
    SkillColumn::Duration,
];

impl SkillTab {
    /// Tab-strip label. Kept to the reference's own wording where it has
    /// one, and to issue #245's where it doesn't.
    pub fn label(self) -> &'static str {
        match self {
            SkillTab::Dps => "Dps",
            SkillTab::Heal => "Heal",
            SkillTab::Buff => "Buff",
            SkillTab::Casts => "Skill casts",
            SkillTab::Dealt => "Skill dealt",
            SkillTab::Received => "Skill received",
        }
    }

    /// This tab's columns, in on-screen order. Fixed slices so the header
    /// row and every data row iterate the identical list — a column can
    /// never appear in one but not the other.
    pub fn columns(self) -> &'static [SkillColumn] {
        match self {
            SkillTab::Dps => &DPS_COLUMNS,
            SkillTab::Heal => &HEAL_COLUMNS,
            SkillTab::Dealt | SkillTab::Received => &AMOUNT_COLUMNS,
            SkillTab::Casts => &CAST_COLUMNS,
            SkillTab::Buff => &BUFF_COLUMNS,
        }
    }

    /// The sort a freshly-shown tab starts on: its amount column,
    /// descending — matching both the reference (every header sets its
    /// amount label's `"↓"` and makes it `_currentSortedLabel`) and the
    /// order `bpsr_meter` already hands the rows over in, so the default
    /// is a no-op on first paint.
    pub fn default_sort(self) -> SkillSort {
        let column = match self {
            SkillTab::Dps => SkillColumn::Damage,
            SkillTab::Heal => SkillColumn::Heal,
            SkillTab::Dealt | SkillTab::Received => SkillColumn::Amount,
            SkillTab::Casts => SkillColumn::Casts,
            // Issue #267: uptime is this tab's "amount", the same role
            // `Damage`/`Heal`/`Amount`/`Casts` play on every other tab.
            SkillTab::Buff => SkillColumn::UptimePct,
        };
        SkillSort {
            column,
            descending: true,
        }
    }

    /// This tab's rows out of one player's snapshot row.
    pub fn rows(self, row: &PlayerRow) -> &[SkillRow] {
        match self {
            SkillTab::Dps => &row.skills,
            SkillTab::Heal => &row.heals,
            SkillTab::Dealt => &row.dealt,
            SkillTab::Received => &row.received,
            SkillTab::Casts => &row.casts,
            SkillTab::Buff => &row.buffs,
        }
    }

    /// Whether anything in the decoder feeds this tab today. A tab that is
    /// not backed is still drawn and still selectable — dimming it out of
    /// existence would leave the user guessing why the reference has a tab
    /// this build doesn't — but it is painted muted and says why when
    /// opened.
    ///
    /// Issue #267 gave `Buff` a real decode path (`AoiSyncDelta.buff_effect`),
    /// so every tab is tracked today. `SkillTab` keeps this method (rather
    /// than callers dropping it as dead) so the next untracked tab has an
    /// obvious place to land.
    pub fn is_tracked(self) -> bool {
        true
    }

    /// What an empty body says. An untracked tab explains the gap rather
    /// than implying the fight simply had none of that; a tracked one falls
    /// through to the caller's live/history wording. Every tab is tracked
    /// today (issue #267), so this always returns `None` — kept, like
    /// `is_tracked`, as the landing spot for the next tab that isn't.
    pub fn untracked_message(self) -> Option<&'static str> {
        None
    }
}

/// Lays out one tab's columns across `available` points of content width
/// (issue #245).
///
/// Every column takes its own [`SkillColumn::width`]; whatever is left over
/// goes to `Name`, the one column with no bounded formatter and so the one
/// that can always use more room. Without this the narrower tabs (`Casts`
/// is four columns totalling 312pt) would pack hard against the window's
/// right edge with a wide dead gap after the icon, because
/// `column_anchors_from_widths` lays columns out right-to-left from the
/// content's right edge.
///
/// Returns the widths unchanged when they already overflow `available` —
/// shrinking them is `column_anchors_from_widths`' job. Shrinking a slot
/// does not shrink the header label painted in it, so that path collides
/// the header text; `SKILL_WINDOW_MIN_SIZE` is what keeps it out of reach,
/// being at least the widest tab's own column sum plus the header row's
/// padding (`skill_window_min_width_fits_every_column_at_its_stated_width`
/// in `ui/skill_window.rs` is what holds that true, and it is only true because the
/// widths above are now measured against their labels — before issue
/// #245's live-window pass they were not, and the labels overlapped at
/// every reachable width).
pub fn column_widths(columns: &[SkillColumn], available: f32) -> Vec<f32> {
    let mut widths: Vec<f32> = columns.iter().map(|c| c.width()).collect();
    let total: f32 = widths.iter().sum();
    let slack = available - total;
    if slack > 0.0
        && let Some(name) = columns.iter().position(|c| *c == SkillColumn::Name)
    {
        widths[name] += slack;
    }
    widths
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
    ///
    /// The arrow is part of what the header row paints, so it is part of
    /// what `SkillColumn::width` has to budget for: appending it to a
    /// width chosen for the bare label is exactly how the sorted column
    /// came to overflow its slot on four of the six tabs (see
    /// `SkillColumn::width`). Every width there now clears this string,
    /// not just `SkillColumn::label`.
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
    // `ui/skill_window.rs` never toggles the sort onto one, so this is belt-and-braces
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

    fn player_row() -> PlayerRow {
        PlayerRow {
            uid: 1,
            name: "Tester".to_owned(),
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: [None, None],
            imagine_tiers: [None, None],
            damage: 0,
            dps: 0.0,
            share_pct: 0.0,
            crit_pct: 0.0,
            lucky_pct: 0.0,
            hits: 0,
            deaths: 0,
            dead_ms: None,
            skills: Vec::new(),
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            buffs: Vec::new(),
        }
    }

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
            SkillColumn::Heal,
            SkillColumn::HealPct,
            SkillColumn::Amount,
            SkillColumn::AmountPct,
            SkillColumn::Casts,
            SkillColumn::CastPerMin,
            SkillColumn::UptimePct,
            SkillColumn::BuffCount,
            SkillColumn::Duration,
        ] {
            assert!(column.sortable(), "{column:?} should be sortable");
        }
    }

    // -- issue #245: tabs -------------------------------------------------

    #[test]
    fn every_tab_but_buff_has_a_name_column_and_a_leading_icon() {
        // Issue #267: `Buff` has a real `Name` column too now, just no
        // leading `Icon` — there is no vendored buff-icon table (see
        // `BUFF_COLUMNS`'s doc comment).
        for tab in SKILL_TABS {
            if tab == SkillTab::Buff {
                assert_eq!(tab.columns()[0], SkillColumn::Name, "{tab:?}");
                continue;
            }
            let columns = tab.columns();
            assert_eq!(columns[0], SkillColumn::Icon, "{tab:?}");
            assert_eq!(columns[1], SkillColumn::Name, "{tab:?}");
        }
    }

    #[test]
    fn every_tabs_default_sort_column_is_one_of_its_own_columns() {
        for tab in SKILL_TABS {
            let sort = tab.default_sort();
            assert!(
                tab.columns().contains(&sort.column),
                "{tab:?} defaults to sorting by {:?}, which it does not show",
                sort.column
            );
            assert!(sort.descending, "{tab:?} should default to descending");
        }
    }

    #[test]
    fn every_tabs_columns_fit_the_windows_minimum_content_width() {
        // `SKILL_WINDOW_MIN_SIZE.x` (880) minus the column header row's
        // two `SKILL_HEADER_PAD_X` insets (24) — the budget `ui/skill_window.rs`'s
        // `skill_window_min_width_fits_every_column_at_its_stated_width`
        // pins for the widest tab, applied to all of them. It grew with
        // the widths themselves when issue #245's live-window pass found
        // the header labels overflowing their columns (`SkillColumn::
        // width`), and again at issue #248's `Name` re-measure (184.0, up
        // from 160.0), which took the widest tab's sum 832 -> 856 and the
        // floor 856 -> 880 with it; `ui/skill_window.rs` owns that constant, so this is
        // the mirror of it that this `egui`-free module can assert.
        for tab in SKILL_TABS {
            let total: f32 = tab.columns().iter().map(|c| c.width()).sum();
            assert!(total <= 856.0, "{tab:?} needs {total}pt of column width");
        }
    }

    #[test]
    fn tab_labels_are_unique() {
        let mut labels: Vec<&str> = SKILL_TABS.iter().map(|t| t.label()).collect();
        labels.sort_unstable();
        let count = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), count);
    }

    #[test]
    fn every_tab_is_tracked() {
        // Issue #267 gave `Buff` a real decode path, so nothing in
        // `SKILL_TABS` is untracked any more.
        for tab in SKILL_TABS {
            assert!(tab.is_tracked(), "{tab:?}");
            assert_eq!(tab.untracked_message(), None, "{tab:?}");
        }
    }

    #[test]
    fn each_tab_reads_its_own_slice_of_the_player_row() {
        let mut row = player_row();
        row.skills = vec![skill_row(1, 10)];
        row.heals = vec![skill_row(2, 20), skill_row(3, 30)];
        row.dealt = vec![skill_row(4, 40)];
        row.received = vec![skill_row(5, 50)];
        assert_eq!(SkillTab::Dps.rows(&row).len(), 1);
        assert_eq!(SkillTab::Heal.rows(&row)[1].skill_id, 3);
        assert_eq!(SkillTab::Dealt.rows(&row)[0].skill_id, 4);
        assert_eq!(SkillTab::Received.rows(&row)[0].skill_id, 5);
        row.casts = vec![skill_row(6, 0)];
        assert_eq!(SkillTab::Casts.rows(&row)[0].skill_id, 6);
        row.buffs = vec![skill_row(7, 70)];
        assert_eq!(SkillTab::Buff.rows(&row)[0].skill_id, 7);
    }

    #[test]
    fn the_amount_columns_read_the_same_fields_the_damage_ones_do() {
        let mut row = skill_row(1, 4242);
        row.share_pct = 25.0;
        row.hits = 7;
        row.hits_per_min = 3.5;
        assert_eq!(
            SkillColumn::Amount.text(&row),
            SkillColumn::Damage.text(&row)
        );
        assert_eq!(SkillColumn::Heal.text(&row), SkillColumn::Damage.text(&row));
        assert_eq!(
            SkillColumn::AmountPct.text(&row),
            SkillColumn::DmgPct.text(&row)
        );
        assert_eq!(SkillColumn::Casts.text(&row), SkillColumn::Hits.text(&row));
        assert_eq!(
            SkillColumn::CastPerMin.text(&row),
            SkillColumn::HitPerMin.text(&row)
        );
    }

    #[test]
    fn sorting_a_heal_tab_by_its_own_amount_column_works() {
        let mut rows = vec![skill_row(1, 100), skill_row(2, 300), skill_row(3, 200)];
        sort_rows(&mut rows, SkillTab::Heal.default_sort());
        let ids: Vec<i32> = rows.iter().map(|r| r.skill_id).collect();
        assert_eq!(ids, vec![2, 3, 1]);
    }

    // -- issue #245: column widths ----------------------------------------

    #[test]
    fn leftover_content_width_goes_to_the_name_column() {
        let columns = SkillTab::Casts.columns();
        let stated: f32 = columns.iter().map(|c| c.width()).sum();
        let widths = column_widths(columns, 728.0);
        assert_eq!(widths.len(), columns.len());
        assert!((widths.iter().sum::<f32>() - 728.0).abs() < 0.01);
        // Only `Name` grew.
        for (i, column) in columns.iter().enumerate() {
            if *column == SkillColumn::Name {
                assert!((widths[i] - (column.width() + 728.0 - stated)).abs() < 0.01);
            } else {
                assert!((widths[i] - column.width()).abs() < 0.01);
            }
        }
    }

    #[test]
    fn column_widths_are_left_alone_when_they_already_overflow() {
        let columns = SkillTab::Dps.columns();
        let widths = column_widths(columns, 100.0);
        for (i, column) in columns.iter().enumerate() {
            assert!((widths[i] - column.width()).abs() < 0.01);
        }
    }

    #[test]
    fn only_the_name_column_is_left_aligned() {
        assert!(SkillColumn::Name.left_aligned());
        for column in [
            SkillColumn::Damage,
            SkillColumn::Heal,
            SkillColumn::Amount,
            SkillColumn::Casts,
            SkillColumn::HitPerMin,
        ] {
            assert!(!column.left_aligned(), "{column:?}");
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

    /// Distinct skill ids observed across a real 9-encounter capture on a
    /// live server (`encounter_player_skills` in the meter's own
    /// `history.sqlite`), sorted. This is the population issue #247 is about:
    /// what the breakdown window is actually asked to draw, as opposed to
    /// what the generated table happens to contain. Extend it, never prune
    /// it — an id that dropped out of coverage is exactly the regression the
    /// assertion below exists to catch.
    const OBSERVED_SKILL_IDS: &[i32] = &[
        1_203, 1_216, 1_222, 1_223, 1_248, 1_259, 1_261, 1_262, 1_263, 1_401, 1_402, 1_403, 1_404,
        1_409, 1_411, 1_418, 1_420, 1_421, 1_422, 1_427, 1_434, 1_435, 1_501, 1_502, 1_503, 1_504,
        1_509, 1_529, 1_541, 1_550, 1_551, 1_561, 1_601, 1_602, 1_603, 1_604, 1_605, 1_607, 1_608,
        1_609, 1_610, 1_611, 1_612, 1_618, 1_619, 1_620, 1_701, 1_702, 1_703, 1_713, 1_715, 1_719,
        1_724, 1_732, 1_737, 1_738, 1_739, 1_740, 1_741, 1_742, 1_901, 1_902, 1_903, 1_904, 1_907,
        1_922, 1_924, 1_927, 1_930, 1_935, 1_937, 1_939, 1_940, 1_941, 1_942, 2_233, 2_240, 2_289,
        2_292, 2_294, 2_295, 2_301, 2_302, 2_303, 2_304, 2_312, 2_313, 2_330, 2_332, 2_336, 2_352,
        2_362, 2_366, 2_401, 2_402, 2_403, 2_404, 2_406, 2_407, 2_410, 2_416, 2_417, 2_421, 2_426,
        2_453, 3_614, 7_998, 21_414, 21_424, 31_901, 35_107, 35_108, 35_109, 44_701, 50_036,
        50_049, 55_231, 55_240, 55_404, 55_417, 55_432, 111_069, 120_201, 120_301, 120_401,
        120_501, 120_901, 120_902, 121_302, 121_501, 140_145, 140_301, 140_401, 149_901, 150_101,
        150_103, 150_104, 150_106, 150_107, 150_110, 160_102, 170_112, 179_906, 179_908, 199_902,
        199_903, 220_101, 220_102, 220_103, 220_104, 220_105, 220_106, 220_108, 220_109, 220_110,
        220_111, 220_113, 220_203, 220_301, 230_401, 230_501, 230_801, 230_901, 231_001, 240_102,
        1_005_240, 1_006_940, 1_007_741, 1_008_440, 1_011_011, 1_100_740, 1_102_205, 1_121_508,
        1_700_440, 1_700_820, 1_700_825, 1_700_826, 1_700_827, 2_002_441, 2_002_853, 2_031_101,
        2_031_102, 2_031_103, 2_031_104, 2_031_105, 2_031_107, 2_031_109, 2_031_110, 2_031_111,
        2_110_099, 2_110_130, 2_201_240, 2_201_540, 2_201_570, 2_201_640, 2_203_091, 2_203_101,
        2_203_102, 2_203_141, 2_203_291, 2_203_521, 2_203_531, 2_203_621, 2_203_622, 2_204_081,
        2_205_060, 2_205_071, 2_206_243, 2_206_290, 2_206_552, 2_208_172, 2_208_181, 2_900_740,
        2_900_840, 3_003_213, 3_054_440, 3_210_051, 3_210_231, 10_310_051,
    ];

    /// How many of `OBSERVED_SKILL_IDS` resolve all the way to a committed
    /// PNG — the only definition of "has an icon" the user can see.
    fn observed_ids_with_a_painted_icon() -> usize {
        OBSERVED_SKILL_IDS
            .iter()
            .filter(|&&id| {
                skill_icon_basename(id)
                    .is_some_and(|icon| crate::skill_icons::SKILL_ICON_FILES.contains(&icon))
            })
            .count()
    }

    #[test]
    fn most_observed_skill_ids_paint_a_real_icon_not_the_blank_disc() {
        // Issue #247: keying the table off BPSR-ZDPS's *curated*
        // `SkillOverrides.en.json` alone covered only 129 of these 219 ids
        // (58.9%); backfilling it from the full client `SkillTable.json` and
        // re-vendoring the art takes it to 155 (70.8%). The floor is set at
        // 65% — under the current figure with enough headroom that a game
        // patch re-iding a handful of skills is a nag, not a red build, but
        // high enough that dropping back to the curated-only table (58.9%)
        // fails immediately.
        //
        // It cannot reach 100%: the rest are proc/DoT damage sources that
        // upstream itself leaves iconless (2031103 "Lucky Strike (Battle
        // Axe)" is a `BuffTable.json` row with `Icon: ""`), so there is no
        // art anywhere to draw. Those keep the blank placeholder by design.
        let covered = observed_ids_with_a_painted_icon();
        let pct = 100.0 * covered as f64 / OBSERVED_SKILL_IDS.len() as f64;
        assert!(
            pct >= 65.0,
            "only {covered}/{} observed skill ids ({pct:.1}%) resolve to a committed icon; \
             regenerate with `python3 scripts/gen-name-tables.py` and re-vendor with \
             `python3 scripts/prep-skill-icons.py <BPSR-ZDPS>/Data/Images`",
            OBSERVED_SKILL_IDS.len(),
        );
    }

    #[test]
    fn the_observed_id_fixture_is_sorted_and_free_of_duplicates() {
        // Guards the fixture itself: a pasted-in duplicate would quietly
        // double-weight one skill in the coverage rate above.
        assert!(OBSERVED_SKILL_IDS.windows(2).all(|w| w[0] < w[1]));
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
        // nothing to derive a glyph from — `ui/skill_window.rs` keeps painting the flat
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
