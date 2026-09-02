//! Persisted user settings: which stat columns the meter renders (issue #13)
//! and where the overlay window sits (issue #27).
//!
//! Lives entirely at the UI layer — no meter/pipeline involvement. Loaded
//! once at startup, then written by two paths: a change from the settings
//! menu, and a drag that moves the overlay window. Both go through the
//! settings-writer thread rather than saving inline (`spawn_writer`).

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use egui::Color32;

use crate::custom_image::ImageSlot;
use crate::ui::{
    CRIT_PCT_RGB, DEATH_COUNT_RGB, LUCKY_PCT_RGB, STAT_TEXT_RGB, StatColumn, fmt_pct0, fmt_share,
    fmt_short,
};

/// One selectable stat column. Declaration order here is also the
/// canonical left-to-right column order used whenever more than one is
/// enabled, regardless of the order columns were toggled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnKind {
    AbilityScore,
    SeasonStrength,
    Damage,
    Dps,
    SharePct,
    CritPct,
    LuckyPct,
    Hits,
    Deaths,
}

impl ColumnKind {
    /// Every selectable column, in canonical left-to-right order.
    ///
    /// `AbilityScore` and `SeasonStrength` lead the list rather than
    /// sitting among the combat-derived columns: each is a static
    /// per-player character stat (a gear-score/season-progression
    /// snapshot, not something that accrues over the fight like
    /// damage/hits do), so they read better next to the row's name than
    /// mixed in with `Damage`/`Dps`/etc.
    ///
    /// `Deaths` (issue #49) closes the list, so it is the *rightmost*
    /// column whenever it is enabled — the position the reference render
    /// (`docs/reference/new-shinra-ex.webp`) puts its skull counter in, just
    /// past the percentage. It is also the only column painted as a pill
    /// rather than as bare text (`ui`'s `ColumnEmphasis::Counter`), so
    /// keeping it at the end also keeps that chrome from sitting between two
    /// plain-text columns.
    pub const ALL: [ColumnKind; 9] = [
        ColumnKind::AbilityScore,
        ColumnKind::SeasonStrength,
        ColumnKind::Damage,
        ColumnKind::Dps,
        ColumnKind::SharePct,
        ColumnKind::CritPct,
        ColumnKind::LuckyPct,
        ColumnKind::Hits,
        ColumnKind::Deaths,
    ];

    /// Label shown next to this column's checkbox in the settings menu.
    pub fn label(self) -> &'static str {
        match self {
            ColumnKind::AbilityScore => "Ability Score",
            ColumnKind::SeasonStrength => "Season Strength",
            ColumnKind::Damage => "Damage",
            ColumnKind::Dps => "DPS",
            ColumnKind::SharePct => "Share %",
            ColumnKind::CritPct => "Crit %",
            ColumnKind::LuckyPct => "Lucky %",
            ColumnKind::Hits => "Hits",
            ColumnKind::Deaths => "Deaths",
        }
    }

    /// Whether this column renders its value inline with the player's name
    /// (issue #168) rather than in its own reserved stat-column slot. Only
    /// `AbilityScore`/`SeasonStrength` do — the doc comment on `ALL` above
    /// already treats the two as a different kind of column, a static
    /// gear-score/season-progression snapshot that "reads better next to
    /// the row's name" than the combat-derived columns; issue #168 acts on
    /// that by moving them from painting *next to* the name (a leading
    /// stat column) to painting *as part of* the name text itself.
    /// `Settings::stat_columns` uses this to exclude both from the
    /// reserved-width stat-column layout while enabled; `ui`'s
    /// `name_suffix` composes the value each one leaves behind into the
    /// name slot instead.
    pub fn renders_inline_with_name(self) -> bool {
        matches!(self, ColumnKind::AbilityScore | ColumnKind::SeasonStrength)
    }

    /// This column's fixed on-screen width plus the formatter that renders
    /// its value, handed over together in one `StatColumn` so the two can
    /// never be wired up independently — a new `ColumnKind` cannot reserve
    /// space without also saying what gets painted into it, or vice versa.
    ///
    /// `width` is the space this column reserves to its own *left* (issue
    /// #8's anchor scheme), budgeted for the widest text `text` can
    /// produce; `ui`'s `widest_formatted_text_fits_its_column_width_budget`
    /// holds every column here to that budget.
    pub fn spec(self) -> StatColumn {
        match self {
            // `None` (no FIGHT_POINT packet seen yet for this player) is a
            // blank cell, not "0" — a missing reading is not the same as a
            // zero score. Full, un-abbreviated figure (owner requirement)
            // rather than `fmt_short`'s compact form — a player's ability
            // score is a single static number worth reading exactly, not a
            // rate/total where a rounded "12.3M" is good enough. No
            // thousands separator: the codebase has no existing formatter
            // for one, and this slice isn't the place to add one. `width`
            // is sized to ability score's real in-game ceiling — a 5-digit
            // stat, max 99_999, per the repo owner, not the field type's
            // own `u32::MAX` ceiling — measured at 39.125pt in
            // `widest_formatted_text_fits_its_column_width_budget` and
            // rounded up to the next multiple of 8, the same small-margin
            // convention `Damage`/`Hits` below use. The field is decoded
            // straight off the packet with no clamp, so a value past this
            // assumed ceiling is still possible — but since issue #168 this
            // column no longer occupies a stat slot while enabled
            // (`renders_inline_with_name`, honoured by
            // `Settings::stat_columns`), so `column_clip_rect` never sees
            // it: its only paint path is `ui::name_suffix`, rendered inline
            // after the player's name and deliberately unclipped and
            // uncapped. An over-ceiling value therefore just makes that
            // suffix wider and bleeds *under* the stat columns (which paint
            // after it) rather than being cut off. `width` stays the budget
            // the formatter is held to by
            // `widest_formatted_text_fits_its_column_width_budget`, and the
            // slot it would reserve again if the inline treatment were ever
            // reverted.
            ColumnKind::AbilityScore => StatColumn {
                width: 40.0,
                text: |row| match row.ability_score {
                    Some(v) => v.to_string(),
                    None => String::new(),
                },
                color: Color32::WHITE,
            },
            // Same full-figure requirement as `AbilityScore` above, but its
            // own independent width: season strength's real in-game
            // ceiling is a 4-digit stat, max 9_999, per the repo owner,
            // measured at 31.3125pt and rounded up to the next multiple of
            // 8. Same unclamped-packet caveat as `AbilityScore` above, and
            // the same inline-suffix path handles it — no clip, no cap,
            // just a wider `ui::name_suffix`.
            ColumnKind::SeasonStrength => StatColumn {
                width: 32.0,
                text: |row| match row.season_strength {
                    Some(v) => v.to_string(),
                    None => String::new(),
                },
                color: Color32::WHITE,
            },
            // Issue #118's 2-/1-decimal bands changed `fmt_short`'s worst
            // case to "99.99M"/"999.9M" (6 chars) — narrower than the old
            // always-one-decimal worst case, "1000.0K" (7 chars, rounds
            // up across a K/M/B threshold), not wider like `Dps` below:
            // `Damage` always used `fmt_short`, never the old `fmt_dps`,
            // so there's no "1000K" (5 chars, `fmt_dps`'s own old worst
            // case) baseline here to widen past. This column's 56.0
            // already had enough slack (the new worst case measures
            // 43.78125pt, per `widest_formatted_text_fits_its_column_
            // width_budget`) that it still holds without changing.
            ColumnKind::Damage => StatColumn {
                width: 56.0,
                text: |row| fmt_short(row.damage),
                color: Color32::from_rgb(STAT_TEXT_RGB.0, STAT_TEXT_RGB.1, STAT_TEXT_RGB.2),
            },
            // Issue #118 unified `fmt_dps` into `fmt_short` — there is only
            // one abbreviator now, shared with `Damage`/`Hits` below.
            // `width` used to be sized for the old `fmt_dps`'s "1000K/s"
            // (7 chars); `fmt_short`'s 2-/1-decimal bands are one char
            // wider ("99.99M/s"/"999.9M/s", 8 chars) than that 0-decimal
            // form, and — per `widest_formatted_text_fits_its_column_
            // width_budget`'s real galley measurement, not a char count —
            // the "M" suffix also happens to be the pixel-widest of
            // K/M/B in this font, so the true worst case is 54.0pt.
            // Measured and rounded up to the next multiple of 8, same
            // convention as the columns around it.
            ColumnKind::Dps => StatColumn {
                width: 56.0,
                text: |row| format!("{}/s", fmt_short(row.dps as i64)),
                color: Color32::WHITE,
            },
            // Unaffected by issue #80.2's 0-decimal change (that's `CritPct`/
            // `LuckyPct` only) — still `fmt_share`'s one decimal, e.g.
            // `100.0%`. `width` is tightened per issue #80.1: measured at
            // 43.34375pt for the widest plausible value and rounded up to
            // the next multiple of 8, same convention as the columns below.
            ColumnKind::SharePct => StatColumn {
                width: 48.0,
                text: |row| fmt_share(row.share_pct),
                color: Color32::from_rgb(STAT_TEXT_RGB.0, STAT_TEXT_RGB.1, STAT_TEXT_RGB.2),
            },
            // 0 decimal places (issue #80.2): the reference render shows
            // `73%`/`76%`/etc., not `fmt_share`'s one-decimal `73.0%`.
            // `width` measured at 32.8125pt for "100%" and rounded up to the
            // next multiple of 8.
            ColumnKind::CritPct => StatColumn {
                width: 40.0,
                text: |row| fmt_pct0(row.crit_pct),
                color: Color32::from_rgb(CRIT_PCT_RGB.0, CRIT_PCT_RGB.1, CRIT_PCT_RGB.2),
            },
            // Same formatter and width reasoning as `CritPct` above.
            ColumnKind::LuckyPct => StatColumn {
                width: 40.0,
                text: |row| fmt_pct0(row.lucky_pct),
                color: Color32::from_rgb(LUCKY_PCT_RGB.0, LUCKY_PCT_RGB.1, LUCKY_PCT_RGB.2),
            },
            // `fmt_short` bounds this to ~7 chars regardless of how many
            // hits land, so it shares `Damage`/`Dps`'s width instead of
            // growing without limit like a raw `to_string()` would. (A
            // wider-than-budget value would still be safe — `draw_row`
            // clips each column's paint to its own slot — but staying
            // within budget is what keeps the text from being cut off in
            // the first place.)
            ColumnKind::Hits => StatColumn {
                width: 56.0,
                text: |row| fmt_short(row.hits as i64),
                color: Color32::WHITE,
            },
            // Death count (issue #49). The plain count, un-abbreviated: a
            // wipe-count is a 1-2 digit figure, so `fmt_short` would only
            // ever hand back the same digits with more code between them.
            //
            // `width` is the odd one out in this match: this is the only
            // column `draw_row` paints as an oval `stat_pill` (a skull glyph,
            // a gap, then the count) rather than as bare text, so its budget
            // has to cover the whole pill — `2 * PILL_PAD_X` + icon +
            // `PILL_ICON_GAP` + the digits — not just the string.
            // `ui`'s `deaths_column_width_fits_the_whole_counter_pill` is
            // what holds this number to that, the same way
            // `widest_formatted_text_fits_its_column_width_budget` holds
            // every text-only column to its own. Measured at ~39pt for the
            // widest plausible count ("99") and rounded up to the next
            // multiple of 8, the same small-margin convention the columns
            // above use.
            ColumnKind::Deaths => StatColumn {
                width: 48.0,
                text: |row| row.deaths.to_string(),
                color: Color32::from_rgb(DEATH_COUNT_RGB.0, DEATH_COUNT_RGB.1, DEATH_COUNT_RGB.2),
            },
        }
    }
}

/// User-configurable settings, persisted to `%APPDATA%\ShinraMeter-BPSR\settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub visible_columns: Vec<ColumnKind>,
    /// Last on-screen window position (issue #27), applied via
    /// `ViewportBuilder::with_position` on the next launch, or `None` if the
    /// window has never been dragged (or this predates the field). The
    /// `#[serde(default)]` is what lets a settings.json written before issue
    /// #27 (with no `window_position` key at all) keep deserializing instead
    /// of erroring on a missing field — the only backward-compat guarantee
    /// this change makes.
    #[serde(default)]
    pub window_position: Option<[f32; 2]>,
    /// Last on-screen inner (content) window size (issue #134), applied via
    /// `ViewportBuilder::with_inner_size` on the next launch, or `None` if
    /// the window has never been resized (or this predates the field). Same
    /// `#[serde(default)]` back-compat guarantee as `window_position`: a
    /// settings.json written before issue #134 has no `window_size` key at
    /// all and still deserializes.
    #[serde(default)]
    pub window_size: Option<[f32; 2]>,
    /// Overlay-wide panel opacity (issue #166): a single multiplier applied
    /// to the alpha channel of every *background* surface the overlay
    /// paints — the main panel's fill and border chrome (`ui::PANEL_FILL`/
    /// `ui::PANEL_BORDER_COLOR`, at the `CentralPanel` `Frame` call site)
    /// and, since issue #184, the skill-breakdown window's chrome, tab
    /// strip, Deaths pill and row-hover fills. See
    /// `egui::Color32::gamma_multiply`. Deliberately *not* applied to
    /// row/text alpha, so dragging this down dims the backdrop rather than
    /// the stats drawn on top of it.
    ///
    /// Since issue #182 those background constants are fully opaque at
    /// rest, so this multiplier is the *only* source of transparency and
    /// the slider's endpoints are literal: 1.0 paints a solid panel, 0.0
    /// paints no panel at all.
    ///
    /// Clamped to `OPACITY_MIN..=OPACITY_MAX` (0.0..=1.0) both on every
    /// `set_opacity` call and on load (`sanitized`), so a hand-edited
    /// settings.json cannot hold a value the slider itself can't express.
    /// The floor is 0.0 rather than #166's 0.2 — see `OPACITY_MIN` for why
    /// that is recoverable.
    ///
    /// `#[serde(default = "default_opacity")]` is the same back-compat
    /// guarantee `window_position`/`window_size` make with `#[serde(default)]`:
    /// a settings.json written before issue #166 has no `opacity` key at
    /// all and still deserializes. A named default function is used instead
    /// of `Option`'s bare `None` because `f32` has no single sensible
    /// `Default::default()` here — `0.0` would strip every existing user's
    /// overlay down to bare floating text on their very next launch,
    /// exactly the silent-upgrade regression this field must not cause.
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// OS-level mouse passthrough (issue #167): while `true`, the window
    /// ignores every mouse event *except clicks on the toggle-cluster
    /// click-through button itself*, which stays reachable by design —
    /// applied via `platform::set_click_through` (issue #167 rehash: a
    /// `WM_NCHITTEST` carve-out in `platform::window_proc`, not
    /// `ViewportCommand::MousePassthrough`, which had no way to exempt a
    /// region and so made that very button unreachable), both on
    /// `OverlayApp`'s first frame (`ui::OverlayApp::ui`) and from
    /// `ui::toggle_cluster`'s click-through button. `#[serde(default)]`
    /// gives a settings.json written before this field `false`
    /// (`bool::default()`) — the same value this toggle starts at on a
    /// first launch, so nothing changes for an existing install.
    #[serde(default)]
    pub click_through: bool,
    /// Whether the overlay stays pinned above other windows (issue #167),
    /// applied via `ViewportCommand::WindowLevel` at the same two points
    /// as `click_through`. Mirrors `ui::viewport`'s hardcoded
    /// `.with_always_on_top()` — that builder call is still what a fresh
    /// window opens with, before any `Settings` value has reached it, so
    /// this field's default has to agree with it or the very first frame's
    /// re-apply would immediately contradict the window it just opened.
    /// `#[serde(default = "default_always_on_top")]` rather than plain
    /// `#[serde(default)]`: `bool::default()` is `false`, which would flip
    /// a settings.json written before this field existed to "off" the
    /// instant it loaded, silently changing every existing install's
    /// always-on-top behavior rather than preserving it.
    #[serde(default = "default_always_on_top")]
    pub always_on_top: bool,
    /// Whether finished encounters are persisted to `history.sqlite` at all
    /// (issue #39). `#[serde(default = "default_history_enabled")]` rather
    /// than plain `#[serde(default)]`: `bool::default()` is `false`, which
    /// would silently turn history off for every existing settings.json the
    /// instant this field was added — the same trap `default_always_on_top`
    /// exists to avoid.
    #[serde(default = "default_history_enabled")]
    pub history_enabled: bool,
    /// The `RetentionPolicy::max_encounters` the history thread is spawned
    /// with (issue #39). Named-default rather than `#[serde(default)]` for
    /// the same reason as `history_enabled`: `u32::default()` is `0`, which
    /// `RetentionPolicy` reads as "prune nothing by count" — the opposite of
    /// a sane default for a settings.json that predates this field.
    #[serde(default = "default_history_max_encounters")]
    pub history_max_encounters: u32,
    /// The `RetentionPolicy::max_age_days` the history thread is spawned
    /// with (issue #39). Same named-default reasoning as
    /// `history_max_encounters`: `0` would mean "never age out by day count"
    /// rather than the intended 90-day default.
    #[serde(default = "default_history_max_age_days")]
    pub history_max_age_days: u32,
    /// The `RetentionPolicy::min_duration_ms` the history thread is spawned
    /// with (issue #39). Same named-default reasoning as the other three
    /// history fields: `0` would record every fight, however brief, rather
    /// than the intended 5-second floor.
    #[serde(default = "default_history_min_duration_ms")]
    pub history_min_duration_ms: u64,
    /// Issue #121: an arbitrary local image file to paint behind the header
    /// band instead of the compiled-in gradient wash and oversized emblem
    /// (`ui::draw_header_wash`), or `None` for the default artwork. Loaded,
    /// cover-cropped and cached at runtime by `custom_image`; painted at
    /// `.gamma_multiply(opacity)` like every other background surface, so
    /// the existing opacity slider fades it exactly as it fades
    /// `ui::PANEL_FILL`.
    ///
    /// Absolute, as the native picker hands it over. Nothing validates it
    /// here — a path that has since been moved, deleted or replaced with a
    /// non-image degrades to the default artwork and surfaces the reason in
    /// the settings dropdown (`custom_image::CustomImages::error`), which is
    /// deliberately the *only* consequence: a bad path must never keep the
    /// overlay from starting.
    ///
    /// `#[serde(default)]` gives a settings.json written before this field
    /// `None` — the same back-compat guarantee `window_position` documents.
    #[serde(default)]
    pub header_image: Option<PathBuf>,
    /// Issue #253: the sister of `header_image` for the player-row list
    /// below the header band. Independent of it by design — the issue is
    /// explicit that a user may want one, the other, or both — and subject
    /// to the same loading, opacity and failure rules. See `header_image`.
    #[serde(default)]
    pub backdrop_image: Option<PathBuf>,
}

/// The `opacity` default (issue #233): restores the panel to the ~78%
/// (200/255) look every stable release painted before issue #182 removed
/// `ui::PANEL_FILL`'s baked-in 200/255 alpha. That change moved all
/// transparency into this multiplier and left `PANEL_FILL` fully opaque,
/// but left this default at 1.0 — which silently turned the *default*
/// install from ~78%-opaque to 100%-opaque, a look that never shipped
/// stable before. `200.0 / 255.0` reproduces the old combined alpha
/// (`PANEL_FILL`'s 200/255 times opacity's 1.0) now that `opacity` is the
/// only place transparency lives, matching
/// `docs/reference/new-shinra-ex.webp`.
///
/// This restored default only reaches a fresh install (no settings.json
/// yet) or an explicit "Reset to defaults" — the same scope
/// `Settings::default`'s `visible_columns` comment documents. An existing
/// settings.json written during the post-#182/pre-#233 window already has
/// an `"opacity": 1.0` key on disk (the whole struct gets re-serialized on
/// any window move/resize), so `#[serde(default = "default_opacity")]`
/// never fires for it and that install stays at the 1.0 it last had —
/// intended, since a `1.0` on disk is indistinguishable from someone
/// deliberately choosing full opacity, and silently overriding it would be
/// wrong just as often as it would be right.
fn default_opacity() -> f32 {
    200.0 / 255.0
}

/// `Settings::always_on_top`'s serde default (issue #167) — see that
/// field's doc comment for why this can't just be `#[serde(default)]`.
fn default_always_on_top() -> bool {
    true
}

/// `Settings::history_enabled`'s serde default (issue #39) — see that
/// field's doc comment for why this can't just be `#[serde(default)]`.
fn default_history_enabled() -> bool {
    true
}

/// `Settings::history_max_encounters`'s serde default (issue #39), matching
/// `history::RetentionPolicy::default`'s `max_encounters`.
fn default_history_max_encounters() -> u32 {
    500
}

/// `Settings::history_max_age_days`'s serde default (issue #39), matching
/// `history::RetentionPolicy::default`'s `max_age_days`.
fn default_history_max_age_days() -> u32 {
    90
}

/// `Settings::history_min_duration_ms`'s serde default (issue #39), matching
/// `history::RetentionPolicy::default`'s `min_duration_ms`.
fn default_history_min_duration_ms() -> u64 {
    5_000
}

impl Default for Settings {
    fn default() -> Self {
        // `Deaths` joins the out-of-the-box set (issue #49) because the
        // reference render shows the skull counter on every row — it is part
        // of what the meter looks like, not an opt-in extra. Only *new*
        // installs (and wiped settings files) get it: an existing
        // `settings.json` carries its own `visible_columns` and is left
        // exactly as the user last left it, which is intended.
        Self {
            visible_columns: vec![
                ColumnKind::Dps,
                ColumnKind::CritPct,
                ColumnKind::LuckyPct,
                ColumnKind::Deaths,
            ],
            window_position: None,
            window_size: None,
            opacity: default_opacity(),
            click_through: false,
            always_on_top: true,
            history_enabled: default_history_enabled(),
            history_max_encounters: default_history_max_encounters(),
            history_max_age_days: default_history_max_age_days(),
            history_min_duration_ms: default_history_min_duration_ms(),
            // Issues #121/#253: no custom artwork out of the box — the
            // compiled-in header wash and the bare panel fill are the
            // defaults, and `reset_to_defaults` puts them back.
            header_image: None,
            backdrop_image: None,
        }
    }
}

/// How far (in logical pixels) the window has to move before it counts as a
/// new `window_position`. The coordinates come from live UI reporting and
/// jitter by sub-pixel amounts under fractional DPI scaling without the
/// window having been touched, so exact equality would forward that jitter
/// to the settings-writer channel on every repaint.
const POSITION_EPSILON: f32 = 1.0;

/// Same idiom as `POSITION_EPSILON`, for `window_size`: a separate constant
/// (rather than reusing `POSITION_EPSILON` under a shared name) so a future
/// change to one threshold doesn't silently retune the other — they happen
/// to share a value today only because both come from the same fractional-
/// DPI jitter source.
const SIZE_EPSILON: f32 = 1.0;

impl Settings {
    /// Floor of the `opacity` range: a genuinely transparent backdrop
    /// (issue #182, lowering issue #166's 0.2).
    ///
    /// #166 kept this above zero so the overlay could never make itself
    /// invisible and un-draggable. The replacement guarantee is structural
    /// rather than numeric: `opacity` multiplies *background* fills and
    /// border chrome only (see the `opacity` field doc), never row text,
    /// header icons or pill glyphs. At 0.0 the panel's backdrop is gone but
    /// every glyph still paints at full alpha, so the header is still there
    /// to drag and the gear that owns the slider is still there to click —
    /// the setting stays reversible from inside the overlay, with no chrome
    /// to hunt for. That beats a global hotkey or a tray item: no new input
    /// plumbing to own, nothing that can itself be shadowed or fail, and no
    /// second place for the recovery path to rot.
    pub const OPACITY_MIN: f32 = 0.0;
    /// Ceiling of the `opacity` range: fully opaque. Since issue #182 this
    /// really is opaque — `ui::PANEL_FILL` no longer carries a baked-in
    /// 200/255 that capped the slider's top end at ~78%.
    pub const OPACITY_MAX: f32 = 1.0;

    /// Ceiling clamp for `history_max_encounters` (issue #39): a hand-edited
    /// or corrupted settings.json can't turn "keep history" into "keep an
    /// unbounded number of encounters forever".
    pub const HISTORY_MAX_ENCOUNTERS_CAP: u32 = 10_000;
    /// Ceiling clamp for `history_max_age_days`, same reasoning as
    /// `HISTORY_MAX_ENCOUNTERS_CAP` — ten years, comfortably past any
    /// sensible retention window.
    pub const HISTORY_MAX_AGE_DAYS_CAP: u32 = 3_650;
    /// Ceiling clamp for `history_min_duration_ms`, same reasoning again:
    /// ten minutes is already an absurd floor for "worth recording".
    pub const HISTORY_MIN_DURATION_CAP_MS: u64 = 600_000;

    /// Whether `col` is currently enabled.
    pub fn is_visible(&self, col: ColumnKind) -> bool {
        self.visible_columns.contains(&col)
    }

    /// The enabled columns in canonical left-to-right order (see
    /// `ColumnKind::ALL`), independent of the order they were toggled in.
    pub fn ordered_columns(&self) -> Vec<ColumnKind> {
        ColumnKind::ALL
            .into_iter()
            .filter(|c| self.is_visible(*c))
            .collect()
    }

    /// The enabled columns that still reserve their own stat-column slot
    /// in the row layout — `ordered_columns` minus whichever of
    /// `AbilityScore`/`SeasonStrength` are enabled (issue #168: those two
    /// render inline with the player's name instead, via `ui::
    /// name_suffix`, so they must not also reserve stat-column width and
    /// an anchor via `column_anchors`). `draw_rows` calls this — not
    /// `ordered_columns` — to build the enabled-column set it hands to
    /// `stat_columns_for`/`column_anchors`. Canonical order is preserved,
    /// same guarantee as `ordered_columns`.
    pub fn stat_columns(&self) -> Vec<ColumnKind> {
        self.ordered_columns()
            .into_iter()
            .filter(|c| !c.renders_inline_with_name())
            .collect()
    }

    /// Toggles a column on/off. Refuses to disable the last remaining
    /// visible column, so the row can never end up with nothing to show —
    /// the "all columns disabled" nonsense state guarded against by #13 —
    /// and, since issue #168, also refuses to disable the last column that
    /// still occupies a stat slot.
    ///
    /// The second guard exists because `AbilityScore`/`SeasonStrength` no
    /// longer occupy one (`renders_inline_with_name`): with only the
    /// last-visible check, a user with either of them enabled could switch
    /// the grid columns off one at a time — every single step passing
    /// `len() > 1` — and land on e.g. `[AbilityScore]`, where
    /// `stat_columns` is empty and the row paints as a name plus a
    /// bracketed suffix with the whole stat area blank. Toggling the inline
    /// columns themselves off stays allowed: removing one can never empty
    /// `stat_columns`, so they are exempt from the second guard (only the
    /// last-visible one still applies to them).
    pub fn toggle(&mut self, col: ColumnKind) {
        if self.is_visible(col) {
            let empties_the_stat_grid =
                !col.renders_inline_with_name() && !self.stat_columns().iter().any(|c| *c != col);
            if self.visible_columns.len() > 1 && !empties_the_stat_grid {
                self.visible_columns.retain(|c| *c != col);
            }
        } else {
            self.visible_columns.push(col);
        }
    }

    /// Sets `opacity` to `value`, clamped to `OPACITY_MIN..=OPACITY_MAX`
    /// (issue #166). Mutates in place, unlike `with_window_position_if_
    /// changed`/`with_window_size_if_changed`'s copy-on-change idiom:
    /// those exist to *detect* whether a per-frame report is a real change
    /// worth persisting, but an egui `Slider`'s `changed()` already answers
    /// that question for opacity, so there is no equivalent detection work
    /// left for this method to do.
    pub fn set_opacity(&mut self, value: f32) {
        self.opacity = Self::clamp_opacity(value);
    }

    /// Public entry point for the `opacity` default (issue #203): the
    /// header dropdown's "Reset to defaults" item calls this to restore
    /// full opacity via `set_opacity`, the same way it already calls
    /// `Settings::default()` — indirectly, through `ColumnKind` — for its
    /// window-size math. The free `default_opacity()` function above backs
    /// `#[serde(default)]` and `Default for Settings`, but is private to
    /// this module, so this just re-exposes the same value under the
    /// struct's own namespace instead of a caller reaching for
    /// `Settings::default().opacity` or hardcoding `1.0`.
    pub fn default_opacity() -> f32 {
        default_opacity()
    }

    /// The image configured for `slot` (issues #121, #253), or `None` for
    /// "paint the default artwork". One accessor over the two fields rather
    /// than two call-site `match`es: `ui.rs` builds the settings dropdown's
    /// two rows, and paints the two regions, from the same code
    /// parameterized by `ImageSlot`.
    pub fn background_image(&self, slot: ImageSlot) -> Option<&Path> {
        match slot {
            ImageSlot::Header => self.header_image.as_deref(),
            ImageSlot::Backdrop => self.backdrop_image.as_deref(),
        }
    }

    /// Points `slot` at `path`, or clears it with `None`. In-place, then
    /// the caller clones and sends — the same shape `toggle_click_through`
    /// uses, and for the same reason: this only ever changes on a deliberate
    /// click in the settings dropdown, so there is no per-frame jitter for a
    /// `with_..._if_changed` idiom to gate against.
    ///
    /// Deliberately does no validation. Whether the file exists, is
    /// readable, or decodes is `custom_image`'s question, answered at paint
    /// time and reported back through the dropdown; answering it here would
    /// only mean answering it twice and disagreeing whenever the file
    /// changed between the two.
    pub fn set_background_image(&mut self, slot: ImageSlot, path: Option<PathBuf>) {
        match slot {
            ImageSlot::Header => self.header_image = path,
            ImageSlot::Backdrop => self.backdrop_image = path,
        }
    }

    /// Issue #121: restores every user customization this struct carries.
    ///
    /// Literally `*self = Settings::default()`, and that is the point — the
    /// header dropdown's "Reset to defaults" item (issue #203) previously
    /// reset only `opacity` (plus a viewport resize command that never
    /// touched this struct at all), so each field added since then silently
    /// escaped the reset. Assigning the whole struct means a field added
    /// tomorrow is covered the day it lands, with no list here to keep in
    /// sync; `reset_to_defaults_restores_every_field` is what holds that.
    ///
    /// That includes `window_position`/`window_size`: both go back to
    /// `None`, i.e. "nothing saved". The live window is not moved by this
    /// (the caller sends its own `ViewportCommand::InnerSize`), and the very
    /// next frame's position/size report repopulates them — which is
    /// correct, since what the user asked to discard is the *customization*,
    /// not the window they are looking at.
    pub fn reset_to_defaults(&mut self) {
        *self = Self::default();
    }

    /// Clamps `value` into `OPACITY_MIN..=OPACITY_MAX`. Handles non-finite
    /// input explicitly: `f32::clamp` compares with `<`/`>`, both of which
    /// are false against NaN, so `NaN.clamp(min, max)` would otherwise pass
    /// NaN straight through instead of landing in range.
    fn clamp_opacity(value: f32) -> f32 {
        if !value.is_finite() {
            return Self::OPACITY_MAX;
        }
        value.clamp(Self::OPACITY_MIN, Self::OPACITY_MAX)
    }

    /// Flips `click_through` (issue #167) — see its field doc comment for
    /// what it drives. In-place-then-clone-and-send is the same shape
    /// `draw_header_menu`'s column checkboxes use around `toggle` above,
    /// rather than the report-every-frame `with_..._if_changed` idiom
    /// `window_position`/`window_size` use below: this only ever changes on
    /// a deliberate button click, or the tray menu's "Turn off
    /// click-through" escape hatch (`ui::click_through_after_tray_request`
    /// — issue #167 rehash), so there is no per-frame jitter to gate
    /// against.
    pub fn toggle_click_through(&mut self) {
        self.click_through = !self.click_through;
    }

    /// Flips `always_on_top` (issue #167) — see `toggle_click_through`'s
    /// doc comment for the shape this mirrors and why.
    pub fn toggle_always_on_top(&mut self) {
        self.always_on_top = !self.always_on_top;
    }

    /// Returns an updated copy with `window_position` set to `position`, or
    /// `None` if it is still the same position (issue #27). The overlay
    /// reports its outer position every single frame — including every frame
    /// of a drag gesture — so this is the change-detection gate that keeps
    /// the settings-writer channel from being sent an identical value on
    /// every repaint; only an actual move should result in a send.
    pub fn with_window_position_if_changed(&self, position: [f32; 2]) -> Option<Settings> {
        if let Some(current) = self.window_position
            && (current[0] - position[0]).abs() < POSITION_EPSILON
            && (current[1] - position[1]).abs() < POSITION_EPSILON
        {
            return None;
        }
        let mut updated = self.clone();
        updated.window_position = Some(position);
        Some(updated)
    }

    /// Returns an updated copy with `window_size` set to `size`, or `None`
    /// if it is still the same size (issue #134). Mirrors
    /// `with_window_position_if_changed`'s change-detection gate: the
    /// overlay's inner size is reported every frame, so only an actual
    /// resize should reach the settings-writer channel.
    pub fn with_window_size_if_changed(&self, size: [f32; 2]) -> Option<Settings> {
        if let Some(current) = self.window_size
            && (current[0] - size[0]).abs() < SIZE_EPSILON
            && (current[1] - size[1]).abs() < SIZE_EPSILON
        {
            return None;
        }
        let mut updated = self.clone();
        updated.window_size = Some(size);
        Some(updated)
    }

    /// Repairs a nonsense state (currently: no columns enabled) by resetting
    /// only the offending field, leaving everything else — `window_position`
    /// included — exactly as it was loaded. `toggle` already prevents
    /// reaching this via the settings menu, but a hand-edited or otherwise
    /// malformed settings file could still deserialize into one.
    fn sanitized(mut self) -> Self {
        if self.visible_columns.is_empty() {
            self.visible_columns = Self::default().visible_columns;
        }
        // Issue #166: an out-of-range (or hand-edited-to-NaN) `opacity`
        // must be repaired on load too, not only on the next slider drag —
        // `set_opacity` is not called during deserialization, so this is
        // the only place a loaded value gets clamped before it ever
        // reaches rendering.
        self.opacity = Self::clamp_opacity(self.opacity);
        // Issue #39: same "repair on load, not just on the next edit" logic
        // as `opacity` above — these three never go through a setter that
        // could clamp them, since the spec's DECISION D9 keeps them
        // settings.json-only with no dropdown controls.
        self.history_max_encounters = self
            .history_max_encounters
            .min(Self::HISTORY_MAX_ENCOUNTERS_CAP);
        self.history_max_age_days = self
            .history_max_age_days
            .min(Self::HISTORY_MAX_AGE_DAYS_CAP);
        self.history_min_duration_ms = self
            .history_min_duration_ms
            .min(Self::HISTORY_MIN_DURATION_CAP_MS);
        self
    }

    /// The retention rules the history thread is spawned with (issue #39).
    /// Read once at startup — see the spec's DECISION D9 for why these are
    /// settings.json-only, with no dropdown controls.
    pub fn retention_policy(&self) -> crate::history::RetentionPolicy {
        crate::history::RetentionPolicy {
            max_encounters: self.history_max_encounters,
            max_age_days: self.history_max_age_days,
            min_duration_ms: self.history_min_duration_ms,
        }
    }
}

/// `%APPDATA%\ShinraMeter-BPSR\settings.json`, or `None` if `APPDATA` isn't set
/// (e.g. running outside Windows). `pub(crate)` (rather than private) so the
/// session-bundle export (`crate::bundle`) can find the same file `load`/
/// `save` use, rather than re-deriving the path.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(appdata)
            .join("ShinraMeter-BPSR")
            .join("settings.json"),
    )
}

/// Loads settings from `%APPDATA%\ShinraMeter-BPSR\settings.json`. Falls back to
/// defaults if `APPDATA` isn't set, the file is missing, or it fails to
/// parse — never panics.
pub fn load() -> Settings {
    match settings_path() {
        Some(path) => load_from(&path),
        None => {
            log::warn!("APPDATA not set; using default settings");
            Settings::default()
        }
    }
}

/// Persists settings to `%APPDATA%\ShinraMeter-BPSR\settings.json`. Logs and
/// gives up on any IO error — never panics, never blocks the UI thread on
/// failure.
pub fn save(settings: &Settings) {
    match settings_path() {
        Some(path) => save_to(&path, settings),
        None => log::warn!("APPDATA not set; settings not persisted"),
    }
}

/// Spawns the dedicated settings-writer thread: it owns the settings file
/// and is the only place `save` is called from, keeping the blocking
/// `fs::write` + `fs::rename` off the UI/render thread — the same
/// channel-owning-thread pattern `pipeline::spawn` uses for the meter.
///
/// The UI sends a `Settings` snapshot on every change; the writer coalesces
/// bursts (e.g. several rapid checkbox toggles) by draining the channel down
/// to the newest value before persisting, so a flurry of toggles results in
/// one save of the final state rather than one save per toggle.
pub fn spawn_writer() -> (Sender<Settings>, JoinHandle<()>) {
    spawn_writer_with(save)
}

/// Same as `spawn_writer`, but with the persist step injected — lets tests
/// observe what the writer thread saves without touching the real
/// `%APPDATA%` settings file.
fn spawn_writer_with(
    persist: impl Fn(&Settings) + Send + 'static,
) -> (Sender<Settings>, JoinHandle<()>) {
    let (tx, rx) = unbounded::<Settings>();
    let handle = std::thread::Builder::new()
        .name("settings-writer".to_string())
        .spawn(move || run_writer(rx, persist))
        .expect("failed to spawn the settings-writer thread");
    (tx, handle)
}

/// Blocks on the channel, persisting the newest pending `Settings` each time
/// it wakes up. Returns (and the thread exits) once every `Sender` is
/// dropped.
fn run_writer(rx: Receiver<Settings>, persist: impl Fn(&Settings)) {
    while let Ok(mut settings) = rx.recv() {
        for latest in rx.try_iter() {
            settings = latest;
        }
        persist(&settings);
    }
}

fn load_from(path: &Path) -> Settings {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to read settings at {}: {err}", path.display());
            }
            return Settings::default();
        }
    };
    match serde_json::from_str::<Settings>(&contents) {
        Ok(settings) => settings.sanitized(),
        Err(err) => {
            log::warn!("failed to parse settings at {}: {err}", path.display());
            Settings::default()
        }
    }
}

/// Writes via a temp-file-plus-rename so a crash or power loss mid-write
/// can never leave a half-written file for the next `load` to trip over.
fn save_to(path: &Path, settings: &Settings) {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!(
            "failed to create settings directory {}: {err}",
            parent.display()
        );
        return;
    }
    let json = match serde_json::to_string_pretty(settings) {
        Ok(j) => j,
        Err(err) => {
            log::warn!("failed to serialize settings: {err}");
            return;
        }
    };
    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp_path, json) {
        log::warn!(
            "failed to write settings temp file {}: {err}",
            tmp_path.display()
        );
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        log::warn!("failed to move settings temp file into place: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsr_meter::PlayerRow;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh path per test (and per call within a test), so parallel test
    /// runs never collide on the same file.
    fn temp_settings_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-test-{tag}-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn round_trip_preserves_settings() {
        let path = temp_settings_path("roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::Hits);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let path = temp_settings_path("missing");
        // Deliberately never written.
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        let path = temp_settings_path("corrupt");
        fs::write(&path, b"not valid json{{{").expect("write corrupt fixture");

        assert_eq!(load_from(&path), Settings::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_creates_missing_parent_directory() {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ShinraMeter-BPSR-test-nested-{}-{n}",
            std::process::id()
        ));
        let path = dir.join("settings.json");

        save_to(&path, &Settings::default());

        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_refuses_to_disable_the_last_visible_column() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Damage],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };

        settings.toggle(ColumnKind::Damage);

        assert_eq!(settings.visible_columns, vec![ColumnKind::Damage]);
    }

    /// Issue #168 review: `AbilityScore`/`SeasonStrength` no longer occupy
    /// a stat slot, so the last-visible-column guard alone would let a user
    /// walk the grid columns off one at a time and end up with an empty
    /// `stat_columns` — a row that is just a name and a bracketed suffix.
    #[test]
    fn toggle_refuses_to_disable_the_last_column_that_occupies_a_stat_slot() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Damage, ColumnKind::AbilityScore],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };

        settings.toggle(ColumnKind::Damage);

        assert_eq!(
            settings.visible_columns,
            vec![ColumnKind::Damage, ColumnKind::AbilityScore],
            "removing the last slot-occupying column must be refused"
        );
        assert!(!settings.stat_columns().is_empty());
    }

    /// The walk that reaches that state one legal-looking step at a time:
    /// every intermediate toggle passes `len() > 1`, so only the
    /// stat-slot guard can stop the last one.
    #[test]
    fn toggling_grid_columns_off_one_by_one_still_leaves_a_stat_column() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);

        for col in ColumnKind::ALL {
            if !col.renders_inline_with_name() && settings.is_visible(col) {
                settings.toggle(col);
            }
        }

        assert!(
            !settings.stat_columns().is_empty(),
            "walking every grid column off one at a time must still leave a stat slot: {:?}",
            settings.visible_columns
        );
        assert!(settings.is_visible(ColumnKind::AbilityScore));
    }

    /// The inline columns are exempt from the stat-slot guard — turning
    /// both of them off (and back on) has to keep working, since neither
    /// contributes a slot in the first place.
    #[test]
    fn toggle_still_allows_turning_both_inline_columns_off_and_on() {
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);
        assert!(settings.is_visible(ColumnKind::AbilityScore));
        assert!(settings.is_visible(ColumnKind::SeasonStrength));

        settings.toggle(ColumnKind::AbilityScore);
        settings.toggle(ColumnKind::SeasonStrength);

        assert!(!settings.is_visible(ColumnKind::AbilityScore));
        assert!(!settings.is_visible(ColumnKind::SeasonStrength));
    }

    /// Even with no stat column left to protect, the inline pair stays
    /// toggleable — `stat_columns` is already empty here (a hand-edited
    /// settings file could deserialize into it), so the stat-slot guard
    /// must not latch and only the last-visible-column guard applies.
    #[test]
    fn toggle_allows_disabling_an_inline_column_when_no_stat_column_is_left() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::AbilityScore, ColumnKind::SeasonStrength],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };

        settings.toggle(ColumnKind::SeasonStrength);

        assert_eq!(settings.visible_columns, vec![ColumnKind::AbilityScore]);
    }

    #[test]
    fn ability_score_is_not_visible_by_default() {
        assert!(!Settings::default().is_visible(ColumnKind::AbilityScore));
    }

    #[test]
    fn ability_score_column_round_trips() {
        let path = temp_settings_path("ability-score-roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::AbilityScore);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn settings_json_without_ability_score_still_deserializes() {
        let path = temp_settings_path("legacy-no-ability-score");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write legacy fixture");

        let loaded = load_from(&path);

        // Not `Settings::default()`: the fixture pins an explicit legacy
        // column list, and the default's own columns have since changed
        // independently of this ability-score backward-compat check.
        assert_eq!(
            loaded.visible_columns,
            vec![ColumnKind::Damage, ColumnKind::Dps, ColumnKind::SharePct]
        );
        assert!(!loaded.is_visible(ColumnKind::AbilityScore));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn season_strength_is_not_visible_by_default() {
        assert!(!Settings::default().is_visible(ColumnKind::SeasonStrength));
    }

    #[test]
    fn season_strength_column_round_trips() {
        let path = temp_settings_path("season-strength-roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::SeasonStrength);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn settings_json_without_season_columns_still_deserializes() {
        let path = temp_settings_path("legacy-no-season-columns");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write legacy fixture");

        let loaded = load_from(&path);

        // Not `Settings::default()`: the fixture pins an explicit legacy
        // column list, and the default's own columns have since changed
        // independently of this season-columns backward-compat check.
        assert_eq!(
            loaded.visible_columns,
            vec![ColumnKind::Damage, ColumnKind::Dps, ColumnKind::SharePct]
        );
        assert!(!loaded.is_visible(ColumnKind::SeasonStrength));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn empty_visible_columns_sanitizes_to_default() {
        let settings = Settings {
            visible_columns: vec![],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };
        assert_eq!(settings.sanitized(), Settings::default());
    }

    #[test]
    fn loading_a_hand_edited_empty_column_list_falls_back_to_default() {
        let path = temp_settings_path("empty-columns");
        fs::write(&path, br#"{"visible_columns":[]}"#).expect("write fixture");

        assert_eq!(load_from(&path), Settings::default());
        let _ = fs::remove_file(&path);
    }

    /// Repairing the column list must not throw the rest of the file away:
    /// a settings.json with no columns but a perfectly good window position
    /// still reopens the overlay where it was left.
    #[test]
    fn sanitizing_an_empty_column_list_keeps_every_other_field() {
        let path = temp_settings_path("empty-columns-with-position");
        fs::write(
            &path,
            br#"{"visible_columns":[],"window_position":[321.0,654.0]}"#,
        )
        .expect("write fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.window_position, Some([321.0, 654.0]));
        assert_eq!(loaded.visible_columns, Settings::default().visible_columns);
        let _ = fs::remove_file(&path);
    }

    // -- spawn_writer / run_writer (the settings-writer thread) ----------

    /// Sending one `Settings` value results in exactly one persist call
    /// with that value — the basic wiring works.
    #[test]
    fn writer_persists_a_sent_settings_value() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Settings>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let (tx, handle) = spawn_writer_with(move |s| seen_clone.lock().unwrap().push(s.clone()));

        let mut settings = Settings::default();
        settings.toggle(ColumnKind::Hits);
        tx.send(settings.clone()).expect("writer thread is alive");

        drop(tx);
        handle.join().expect("writer thread should not panic");

        assert_eq!(*seen.lock().unwrap(), vec![settings]);
    }

    /// A burst of sends made faster than the writer can wake up and drain
    /// must coalesce down to the final value rather than persisting every
    /// intermediate one.
    #[test]
    fn writer_coalesces_a_burst_of_sends_to_the_latest_value() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Settings>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let (tx, handle) = spawn_writer_with(move |s| seen_clone.lock().unwrap().push(s.clone()));

        let mut settings = Settings::default();
        for col in [ColumnKind::CritPct, ColumnKind::LuckyPct, ColumnKind::Hits] {
            settings.toggle(col);
            tx.send(settings.clone()).expect("writer thread is alive");
        }

        drop(tx);
        handle.join().expect("writer thread should not panic");

        // The writer may or may not have woken up between individual sends
        // (that race is exactly what coalescing tolerates), but the very
        // last thing persisted must be the final settings state, and
        // nothing after it.
        assert_eq!(seen.lock().unwrap().last(), Some(&settings));
    }

    /// Dropping every `Sender` closes the channel and the thread exits
    /// cleanly instead of blocking forever.
    #[test]
    fn writer_thread_exits_once_the_sender_is_dropped() {
        let (tx, handle) = spawn_writer_with(|_| {});
        drop(tx);
        handle.join().expect("writer thread should not panic");
    }

    // -- window_position (issue #27) --------------------------------------

    #[test]
    fn round_trip_preserves_window_position() {
        let path = temp_settings_path("position-roundtrip");
        let settings = Settings {
            window_position: Some([123.0, 456.0]),
            ..Settings::default()
        };
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issue #27 (no `window_position` key at
    /// all) must still deserialize — the one backward-compat guarantee this
    /// change makes (`#[serde(default)]`, not a migration).
    #[test]
    fn settings_json_without_window_position_key_falls_back_to_none() {
        let path = temp_settings_path("no-position-key");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write pre-#27 fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.window_position, None);
        // Not `Settings::default().visible_columns`: the fixture pins an
        // explicit legacy column list, independent of the current default.
        assert_eq!(
            loaded.visible_columns,
            vec![ColumnKind::Damage, ColumnKind::Dps, ColumnKind::SharePct]
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn with_window_position_if_changed_returns_none_when_unchanged() {
        let settings = Settings {
            window_position: Some([10.0, 20.0]),
            ..Settings::default()
        };

        assert_eq!(settings.with_window_position_if_changed([10.0, 20.0]), None);
    }

    #[test]
    fn with_window_position_if_changed_returns_updated_settings_on_change() {
        let settings = Settings {
            window_position: Some([10.0, 20.0]),
            ..Settings::default()
        };

        let updated = settings
            .with_window_position_if_changed([30.0, 40.0])
            .expect("position changed, should produce an update");

        assert_eq!(updated.window_position, Some([30.0, 40.0]));
        // Everything else carries over unchanged.
        assert_eq!(updated.visible_columns, settings.visible_columns);
    }

    /// Sub-pixel drift under fractional DPI scaling is not a move — the
    /// window sat still and the reported coordinates merely wobbled.
    #[test]
    fn with_window_position_if_changed_ignores_sub_pixel_jitter() {
        let settings = Settings {
            window_position: Some([10.0, 20.0]),
            ..Settings::default()
        };

        assert_eq!(
            settings.with_window_position_if_changed([10.4, 19.6]),
            None,
            "a fraction of a pixel is not a move"
        );
    }

    /// A move of a full logical pixel or more is a real move and must be
    /// persisted.
    #[test]
    fn with_window_position_if_changed_reports_a_one_pixel_move() {
        let settings = Settings {
            window_position: Some([10.0, 20.0]),
            ..Settings::default()
        };

        let updated = settings
            .with_window_position_if_changed([10.0, 21.0])
            .expect("a full pixel of movement should produce an update");

        assert_eq!(updated.window_position, Some([10.0, 21.0]));
    }

    #[test]
    fn with_window_position_if_changed_treats_no_prior_position_as_a_change() {
        let settings = Settings::default();
        assert_eq!(settings.window_position, None);

        let updated = settings
            .with_window_position_if_changed([1.0, 2.0])
            .expect("first-ever position should count as a change");
        assert_eq!(updated.window_position, Some([1.0, 2.0]));
    }

    // -- window_size (issue #134) ------------------------------------------

    #[test]
    fn round_trip_preserves_window_size() {
        let path = temp_settings_path("size-roundtrip");
        let settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issue #134 (no `window_size` key at
    /// all) must still deserialize — the one backward-compat guarantee this
    /// change makes (`#[serde(default)]`, not a migration).
    #[test]
    fn settings_json_without_window_size_key_falls_back_to_none() {
        let path = temp_settings_path("no-size-key");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write pre-#134 fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.window_size, None);
        // Not `Settings::default().visible_columns`: the fixture pins an
        // explicit legacy column list, independent of the current default.
        assert_eq!(
            loaded.visible_columns,
            vec![ColumnKind::Damage, ColumnKind::Dps, ColumnKind::SharePct]
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn with_window_size_if_changed_returns_none_when_unchanged() {
        let settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        assert_eq!(settings.with_window_size_if_changed([640.0, 480.0]), None);
    }

    #[test]
    fn with_window_size_if_changed_returns_updated_settings_on_change() {
        let settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        let updated = settings
            .with_window_size_if_changed([800.0, 600.0])
            .expect("size changed, should produce an update");

        assert_eq!(updated.window_size, Some([800.0, 600.0]));
        // Everything else carries over unchanged.
        assert_eq!(updated.visible_columns, settings.visible_columns);
    }

    /// Sub-pixel drift under fractional DPI scaling is not a resize — the
    /// window sat still and the reported dimensions merely wobbled.
    #[test]
    fn with_window_size_if_changed_ignores_sub_pixel_jitter() {
        let settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        assert_eq!(
            settings.with_window_size_if_changed([640.4, 479.6]),
            None,
            "a fraction of a pixel is not a resize"
        );
    }

    /// A change of a full logical pixel or more is a real resize and must
    /// be persisted.
    #[test]
    fn with_window_size_if_changed_reports_a_one_pixel_change() {
        let settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        let updated = settings
            .with_window_size_if_changed([640.0, 481.0])
            .expect("a full pixel of resize should produce an update");

        assert_eq!(updated.window_size, Some([640.0, 481.0]));
    }

    #[test]
    fn with_window_size_if_changed_treats_no_prior_size_as_a_change() {
        let settings = Settings::default();
        assert_eq!(settings.window_size, None);

        let updated = settings
            .with_window_size_if_changed([640.0, 480.0])
            .expect("first-ever size should count as a change");
        assert_eq!(updated.window_size, Some([640.0, 480.0]));
    }

    // -- opacity (issue #166) ------------------------------------------

    #[test]
    fn default_opacity_restores_the_pre_182_look() {
        // Issue #233: before issue #182, `ui::PANEL_FILL` carried a baked-in
        // 200/255 alpha, so the rendered default (`default_opacity() == 1.0`
        // multiplying that 200/255) was ~78% opaque. #182 made `PANEL_FILL`
        // fully opaque and moved all transparency into this multiplier, but
        // left the default at 1.0 — silently turning the *default* install
        // from ~78%-opaque to 100%-opaque, a look that never shipped stable
        // before. 200.0 / 255.0 reproduces that original combined alpha now
        // that `opacity` is the only place transparency lives.
        assert_eq!(Settings::default().opacity, 200.0 / 255.0);
    }

    /// Issue #203: the header dropdown's "Reset to defaults" item needs a
    /// public entry point for this value — the free `default_opacity()`
    /// function above is private to this module. It must agree with what a
    /// fresh `Settings::default()` actually carries, not just happen to
    /// repeat `1.0`.
    #[test]
    fn default_opacity_public_fn_matches_the_struct_default() {
        assert_eq!(Settings::default_opacity(), Settings::default().opacity);
    }

    // -- custom background images (issues #121, #253) ---------------------

    /// Both new fields survive a save/load cycle, and survive it
    /// *independently*: #253 is explicit that a user may configure one
    /// region, the other, or both, so a round trip that only ever carried
    /// them together would not prove what the feature promises.
    #[test]
    fn round_trip_preserves_background_images() {
        let path = temp_settings_path("background-images-roundtrip");
        let mut settings = Settings::default();
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("C:/art/header.png")));
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("C:/art/rows.jpg")));
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        assert_eq!(
            loaded.background_image(ImageSlot::Header),
            Some(Path::new("C:/art/header.png"))
        );
        assert_eq!(
            loaded.background_image(ImageSlot::Backdrop),
            Some(Path::new("C:/art/rows.jpg"))
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn round_trip_preserves_one_background_image_without_the_other() {
        let path = temp_settings_path("background-image-header-only");
        let mut settings = Settings::default();
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("rows.webp")));
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded.background_image(ImageSlot::Header), None);
        assert_eq!(
            loaded.background_image(ImageSlot::Backdrop),
            Some(Path::new("rows.webp"))
        );
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issues #121/#253 has neither key and
    /// must still deserialize — the `#[serde(default)]` back-compat
    /// guarantee `window_position` documents.
    #[test]
    fn settings_json_without_image_keys_falls_back_to_no_custom_images() {
        let path = temp_settings_path("no-image-keys");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps"]}"#).expect("write fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.header_image, None);
        assert_eq!(loaded.backdrop_image, None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn clearing_a_background_image_leaves_the_other_slot_alone() {
        let mut settings = Settings::default();
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("a.png")));
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("b.png")));

        settings.set_background_image(ImageSlot::Header, None);

        assert_eq!(settings.background_image(ImageSlot::Header), None);
        assert_eq!(
            settings.background_image(ImageSlot::Backdrop),
            Some(Path::new("b.png"))
        );
    }

    // -- reset to defaults (issue #121) -----------------------------------

    /// Issue #121 asks for a reset that covers "the custom header image,
    /// display/visible columns, window size, etc." — so this mutates
    /// *every* field away from its default and requires the whole struct
    /// back, rather than spot-checking the three the issue happens to name.
    /// That is also what keeps a field added tomorrow from silently
    /// escaping the reset.
    #[test]
    fn reset_to_defaults_restores_every_field() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Hits],
            window_position: Some([12.0, 34.0]),
            window_size: Some([640.0, 480.0]),
            click_through: !Settings::default().click_through,
            always_on_top: !Settings::default().always_on_top,
            history_enabled: !Settings::default().history_enabled,
            history_max_encounters: 1,
            history_max_age_days: 1,
            history_min_duration_ms: 1,
            ..Settings::default()
        };
        settings.set_opacity(0.25);
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("header.png")));
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("rows.png")));
        assert_ne!(settings, Settings::default(), "the fixture must differ");

        settings.reset_to_defaults();

        assert_eq!(settings, Settings::default());
    }

    /// The three the issue names, asserted by name as well, so a future
    /// change that narrowed the reset would fail with a message that says
    /// which promise it broke rather than just "structs differ".
    #[test]
    fn reset_to_defaults_clears_the_customizations_issue_121_names() {
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::SharePct],
            window_size: Some([1234.0, 567.0]),
            ..Settings::default()
        };
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("header.png")));
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("rows.png")));

        settings.reset_to_defaults();

        assert_eq!(
            settings.visible_columns,
            Settings::default().visible_columns
        );
        assert_eq!(settings.window_size, None);
        assert_eq!(settings.background_image(ImageSlot::Header), None);
        assert_eq!(settings.background_image(ImageSlot::Backdrop), None);
        assert_eq!(settings.opacity, Settings::default_opacity());
    }

    /// The reset is what a user reaches for after breaking something, so it
    /// has to survive round-tripping through the file the app actually
    /// reads back on the next launch.
    #[test]
    fn reset_to_defaults_round_trips_as_a_fresh_settings_file() {
        let path = temp_settings_path("reset-roundtrip");
        let mut settings = Settings::default();
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("header.png")));
        settings.set_opacity(0.1);
        settings.reset_to_defaults();
        save_to(&path, &settings);

        assert_eq!(load_from(&path), Settings::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn round_trip_preserves_opacity() {
        let path = temp_settings_path("opacity-roundtrip");
        let mut settings = Settings::default();
        settings.set_opacity(0.6);
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issue #166 (no `opacity` key at all)
    /// must still deserialize — the same `#[serde(default = "...")]`
    /// back-compat guarantee `window_position`/`window_size` make, just
    /// with a named default function instead of `Option`'s `None` since
    /// `f32` has no single sensible `Default::default()` for this field
    /// (`0.0` would render the overlay invisible on upgrade).
    #[test]
    fn settings_json_without_opacity_key_falls_back_to_default() {
        let path = temp_settings_path("no-opacity-key");
        fs::write(&path, br#"{"visible_columns":["Damage","Dps","SharePct"]}"#)
            .expect("write pre-#166 fixture");

        let loaded = load_from(&path);

        // Issue #233: falls back to the restored ~78%-opaque default, not
        // the drifted 1.0 this assertion used to check.
        assert_eq!(loaded.opacity, 200.0 / 255.0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn set_opacity_clamps_below_the_floor() {
        let mut settings = Settings::default();
        settings.set_opacity(-0.5);
        assert_eq!(settings.opacity, Settings::OPACITY_MIN);
    }

    /// Issue #182: 0.0 is a legal value now, not something the setter
    /// quietly lifts to a 0.2 floor. Asserts the stored value rather than
    /// the constant so this fails loudly if the floor is ever raised back
    /// up without revisiting the recovery argument on `OPACITY_MIN`.
    #[test]
    fn set_opacity_allows_a_fully_transparent_value() {
        let mut settings = Settings::default();
        settings.set_opacity(0.0);
        assert_eq!(settings.opacity, 0.0);
    }

    /// The other half of the clamp on load: `sanitized()` must accept a
    /// hand-written `0.0` unchanged (issue #182) the same way it rejects
    /// the out-of-range `5.0` below.
    #[test]
    fn loading_settings_json_with_a_zero_opacity_keeps_it() {
        let path = temp_settings_path("zero-opacity");
        fs::write(&path, br#"{"visible_columns":["Damage"],"opacity":0.0}"#)
            .expect("write fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.opacity, 0.0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn set_opacity_clamps_above_the_ceiling() {
        let mut settings = Settings::default();
        settings.set_opacity(2.5);
        assert_eq!(settings.opacity, Settings::OPACITY_MAX);
    }

    /// Guards the pure clamp helper directly against a NaN drag value (an
    /// egui `Slider` should never hand one back, but the clamp is the last
    /// line of defense against ending up with an unusable, comparison-proof
    /// opacity either way — `NaN.clamp()` alone would silently pass NaN
    /// through unchanged).
    #[test]
    fn set_opacity_clamps_a_nan_value() {
        let mut settings = Settings::default();
        settings.set_opacity(f32::NAN);
        assert_eq!(settings.opacity, Settings::OPACITY_MAX);
    }

    /// A hand-edited (or otherwise corrupted) settings.json with an
    /// out-of-range `opacity` must be clamped on load, not merely on the
    /// next slider drag — `sanitized()` is what `load_from` always runs a
    /// freshly deserialized `Settings` through.
    #[test]
    fn loading_settings_json_with_an_out_of_range_opacity_clamps_it() {
        let path = temp_settings_path("out-of-range-opacity");
        fs::write(&path, br#"{"visible_columns":["Damage"],"opacity":5.0}"#)
            .expect("write fixture");

        let loaded = load_from(&path);

        assert_eq!(loaded.opacity, Settings::OPACITY_MAX);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ordered_columns_follows_canonical_order_regardless_of_toggle_order() {
        let mut a = Settings {
            visible_columns: vec![ColumnKind::Damage],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };
        a.toggle(ColumnKind::Hits);
        a.toggle(ColumnKind::CritPct);

        let mut b = Settings {
            visible_columns: vec![ColumnKind::Damage],
            window_position: None,
            window_size: None,
            ..Settings::default()
        };
        b.toggle(ColumnKind::CritPct);
        b.toggle(ColumnKind::Hits);

        assert_eq!(a.ordered_columns(), b.ordered_columns());
        assert_eq!(
            a.ordered_columns(),
            vec![ColumnKind::Damage, ColumnKind::CritPct, ColumnKind::Hits]
        );
    }

    // -- inline name-suffix columns excluded from the layout set (#168) ---

    #[test]
    fn stat_columns_excludes_ability_score_and_season_strength_when_visible() {
        let settings = Settings {
            visible_columns: ColumnKind::ALL.to_vec(),
            window_position: None,
            window_size: None,
            ..Settings::default()
        };
        let stat_columns = settings.stat_columns();
        assert!(!stat_columns.contains(&ColumnKind::AbilityScore));
        assert!(!stat_columns.contains(&ColumnKind::SeasonStrength));
        // Every other enabled column is still present, same canonical
        // order `ordered_columns` would give it.
        assert_eq!(
            stat_columns,
            vec![
                ColumnKind::Damage,
                ColumnKind::Dps,
                ColumnKind::SharePct,
                ColumnKind::CritPct,
                ColumnKind::LuckyPct,
                ColumnKind::Hits,
                ColumnKind::Deaths,
            ]
        );
    }

    #[test]
    fn stat_columns_matches_ordered_columns_when_neither_inline_column_is_visible() {
        // Neither `AbilityScore` nor `SeasonStrength` is in the default
        // set, so the two methods agree for it — `stat_columns` only
        // diverges from `ordered_columns` once one of the two is toggled
        // on (the test above).
        assert_eq!(
            Settings::default().stat_columns(),
            Settings::default().ordered_columns()
        );
    }

    // -- default-columns rework: Dps/CritPct/LuckyPct, with color ---------

    /// The out-of-the-box column set, in order. `Deaths` was appended by
    /// issue #49 (the reference render shows the skull counter on every row);
    /// the three columns before it are untouched.
    #[test]
    fn default_columns_are_dps_crit_lucky_deaths_in_order() {
        assert_eq!(
            Settings::default().visible_columns,
            vec![
                ColumnKind::Dps,
                ColumnKind::CritPct,
                ColumnKind::LuckyPct,
                ColumnKind::Deaths
            ]
        );
    }

    #[test]
    fn crit_pct_spec_is_colored_red() {
        assert_eq!(
            ColumnKind::CritPct.spec().color,
            Color32::from_rgb(CRIT_PCT_RGB.0, CRIT_PCT_RGB.1, CRIT_PCT_RGB.2)
        );
    }

    #[test]
    fn lucky_pct_spec_is_colored_green() {
        assert_eq!(
            ColumnKind::LuckyPct.spec().color,
            Color32::from_rgb(LUCKY_PCT_RGB.0, LUCKY_PCT_RGB.1, LUCKY_PCT_RGB.2)
        );
    }

    /// `AbilityScore`, `SeasonStrength` and `Hits` have no counterpart in the
    /// source's fixed column sets, so they stay white. `Dps` is the source's
    /// one headline column and stays white too (decision 4).
    #[test]
    fn unbudgeted_columns_and_dps_stay_white() {
        for kind in [
            ColumnKind::AbilityScore,
            ColumnKind::SeasonStrength,
            ColumnKind::Dps,
            ColumnKind::Hits,
        ] {
            assert_eq!(
                kind.spec().color,
                Color32::WHITE,
                "{kind:?} should stay white"
            );
        }
    }

    /// `Damage`/`SharePct` are plain stats in the source (`DamagePercDT`/
    /// `DamageDT`, `Foreground="#aaa"`), stepped down from white (decision 4,
    /// issue #62).
    #[test]
    fn plain_stat_columns_use_the_dim_stat_color() {
        for kind in [ColumnKind::Damage, ColumnKind::SharePct] {
            assert_eq!(
                kind.spec().color,
                Color32::from_rgb(STAT_TEXT_RGB.0, STAT_TEXT_RGB.1, STAT_TEXT_RGB.2),
                "{kind:?} should use STAT_TEXT_RGB"
            );
        }
    }

    #[test]
    fn season_level_variant_no_longer_exists_in_all() {
        assert_eq!(ColumnKind::ALL.len(), 9);
        assert!(ColumnKind::ALL.iter().all(|c| c.label() != "Season Level"));
    }

    // -- death count column (issue #49) -----------------------------------

    /// Every column in `ALL` needs a label and a spec; the compiler enforces
    /// the `match` arms exist, but not that a new variant was actually added
    /// to `ALL` — which is the one list nothing else generates.
    #[test]
    fn deaths_is_in_all_and_has_a_label_and_a_spec() {
        assert!(ColumnKind::ALL.contains(&ColumnKind::Deaths));
        assert_eq!(ColumnKind::Deaths.label(), "Deaths");
        assert!(ColumnKind::Deaths.spec().width > 0.0);
    }

    /// The reference render puts the skull counter at the row's right edge,
    /// past the percentage — so `Deaths` has to be last in the canonical
    /// order, not merely present in it.
    #[test]
    fn deaths_is_the_rightmost_column() {
        assert_eq!(ColumnKind::ALL.last(), Some(&ColumnKind::Deaths));

        let all = Settings {
            visible_columns: ColumnKind::ALL.to_vec(),
            window_position: None,
            window_size: None,
            ..Settings::default()
        };
        assert_eq!(all.ordered_columns().last(), Some(&ColumnKind::Deaths));
    }

    #[test]
    fn deaths_is_visible_by_default() {
        assert!(Settings::default().is_visible(ColumnKind::Deaths));
    }

    /// The plain count, never `fmt_short`'s abbreviation — a wipe count is a
    /// 1-2 digit figure and "1.0K deaths" would be nonsense.
    #[test]
    fn deaths_column_formats_the_plain_count() {
        let row = PlayerRow {
            uid: 7,
            name: String::new(),
            class: None,
            damage: 0,
            dps: 0.0,
            share_pct: 0.0,
            crit_pct: 0.0,
            lucky_pct: 0.0,
            hits: 0,
            deaths: 12,
            dead_ms: Some(0),
            ability_score: None,
            season_strength: None,
            imagines: [None, None],
            imagine_tiers: [None, None],
            skills: Vec::new(),
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            buffs: Vec::new(),
        };
        assert_eq!((ColumnKind::Deaths.spec().text)(&row), "12");
    }

    /// The death count's digits are plain white (decision 4, issue #62): the
    /// source's `DeathsDT` is `Foreground="White"`, and the pill's own
    /// `#1fff` background is what separates the counter from the row rather
    /// than a dimmer digit color.
    #[test]
    fn deaths_spec_is_white() {
        let color = ColumnKind::Deaths.spec().color;
        assert_eq!(
            color,
            Color32::from_rgb(DEATH_COUNT_RGB.0, DEATH_COUNT_RGB.1, DEATH_COUNT_RGB.2)
        );
        assert_eq!(color, Color32::WHITE);
    }

    #[test]
    fn deaths_column_round_trips() {
        let path = temp_settings_path("deaths-roundtrip");
        let mut settings = Settings::default();
        settings.toggle(ColumnKind::Deaths);
        assert!(!settings.is_visible(ColumnKind::Deaths));
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issue #49 has no `"Deaths"` entry at
    /// all; it must keep deserializing (and keep its own column list) rather
    /// than being migrated or rejected.
    #[test]
    fn settings_json_without_deaths_still_deserializes() {
        let path = temp_settings_path("legacy-no-deaths");
        fs::write(
            &path,
            br#"{"visible_columns":["Dps","CritPct","LuckyPct"]}"#,
        )
        .expect("write legacy fixture");

        let loaded = load_from(&path);

        assert_eq!(
            loaded.visible_columns,
            vec![ColumnKind::Dps, ColumnKind::CritPct, ColumnKind::LuckyPct]
        );
        assert!(!loaded.is_visible(ColumnKind::Deaths));
        let _ = fs::remove_file(&path);
    }

    // -- click-through / always-on-top (issue #167) ------------------------

    /// Click-through starts off — a fresh install must never open with the
    /// window unclickable before the user has ever touched the toggle.
    #[test]
    fn default_click_through_is_off() {
        assert!(!Settings::default().click_through);
    }

    /// Always-on-top starts on, matching `ui::viewport`'s hardcoded
    /// `.with_always_on_top()` — the window a fresh process opens with is
    /// already pinned, so the settings default has to agree or the very
    /// first frame's re-apply (`OverlayApp::ui`) would immediately fight
    /// the window it just opened.
    #[test]
    fn default_always_on_top_is_on() {
        assert!(Settings::default().always_on_top);
    }

    #[test]
    fn toggle_click_through_flips_the_flag() {
        let mut settings = Settings::default();
        assert!(!settings.click_through);

        settings.toggle_click_through();
        assert!(settings.click_through);

        settings.toggle_click_through();
        assert!(!settings.click_through);
    }

    #[test]
    fn toggle_click_through_leaves_other_fields_unchanged() {
        let mut settings = Settings::default();
        settings.toggle_click_through();

        assert_eq!(settings.always_on_top, Settings::default().always_on_top);
        assert_eq!(
            settings.visible_columns,
            Settings::default().visible_columns
        );
    }

    #[test]
    fn toggle_always_on_top_flips_the_flag() {
        let mut settings = Settings::default();
        assert!(settings.always_on_top);

        settings.toggle_always_on_top();
        assert!(!settings.always_on_top);

        settings.toggle_always_on_top();
        assert!(settings.always_on_top);
    }

    #[test]
    fn round_trip_preserves_click_through_and_always_on_top() {
        let path = temp_settings_path("toggles-roundtrip");
        let mut settings = Settings::default();
        settings.toggle_click_through();
        settings.toggle_always_on_top();
        save_to(&path, &settings);

        let loaded = load_from(&path);

        assert_eq!(loaded, settings);
        assert!(loaded.click_through);
        assert!(!loaded.always_on_top);
        let _ = fs::remove_file(&path);
    }

    /// A settings.json written before issue #167 has neither key at all; it
    /// must keep deserializing, with `click_through` falling back to `false`
    /// and `always_on_top` falling back to `true` — the same back-compat
    /// guarantee `window_position`/`window_size` make, and the only one
    /// `#[serde(default = ...)]` is for (never a migration).
    #[test]
    fn settings_json_without_toggle_keys_falls_back_to_their_defaults() {
        let path = temp_settings_path("legacy-no-toggles");
        fs::write(
            &path,
            br#"{"visible_columns":["Dps","CritPct","LuckyPct"]}"#,
        )
        .expect("write legacy fixture");

        let loaded = load_from(&path);

        assert!(!loaded.click_through);
        assert!(loaded.always_on_top);
        let _ = fs::remove_file(&path);
    }

    // -- history (issue #39) --------------------------------------------

    /// A settings.json predating issue #39 (no history keys at all) must
    /// still deserialize, with every history field falling back to its
    /// documented default — the same back-compat guarantee
    /// `always_on_top`/`click_through` make.
    #[test]
    fn settings_json_without_the_history_fields_still_loads() {
        let path = temp_settings_path("legacy-no-history");
        fs::write(
            &path,
            br#"{"visible_columns":["Dps","CritPct","LuckyPct"]}"#,
        )
        .expect("write legacy fixture");

        let loaded = load_from(&path);

        assert!(loaded.history_enabled);
        assert_eq!(loaded.history_max_encounters, 500);
        assert_eq!(loaded.history_max_age_days, 90);
        assert_eq!(loaded.history_min_duration_ms, 5_000);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn an_absurd_history_cap_is_clamped_on_load() {
        let settings = Settings {
            history_max_encounters: 1_000_000,
            history_max_age_days: 100_000,
            history_min_duration_ms: 10_000_000,
            ..Settings::default()
        }
        .sanitized();

        assert_eq!(
            (
                settings.history_max_encounters,
                settings.history_max_age_days,
                settings.history_min_duration_ms
            ),
            (
                Settings::HISTORY_MAX_ENCOUNTERS_CAP,
                Settings::HISTORY_MAX_AGE_DAYS_CAP,
                Settings::HISTORY_MIN_DURATION_CAP_MS
            )
        );
    }

    #[test]
    fn retention_policy_mirrors_the_settings_fields() {
        let settings = Settings {
            history_max_encounters: 42,
            history_max_age_days: 7,
            history_min_duration_ms: 1_234,
            ..Settings::default()
        };

        let policy = settings.retention_policy();

        assert_eq!(
            (
                policy.max_encounters,
                policy.max_age_days,
                policy.min_duration_ms
            ),
            (42, 7, 1_234)
        );
    }
}
