//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in
//! — with two deliberate exceptions, both header-menu items: issue #171's
//! "Check for updates" (`draw_header_menu`, `UpdateCheckState`) and issue
//! #220's "Export logs" (`start_log_export`). Each spawns its own one-shot
//! `std::thread` and reports back over a `crossbeam_channel`, the same way
//! `settings::spawn_writer` and `pipeline::spawn` do at the app layer,
//! because the app layer has no channel of its own suited to a single
//! manual, UI-triggered request/reply.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bpsr_meter::{
    Class, EncounterInfo, PlayerRow, Role, SkillRow, SkillStats, Snapshot, skill_row_from_stats,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use eframe::egui;

use crate::fonts;
use crate::history;
use crate::icons::{
    ClassIcons, GlyphIcon, GlyphIcons, ImagineIcons, SkillIcons, ToolbarIcon, ToolbarIcons,
};
use crate::imagines;
use crate::settings::{ColumnKind, Settings};
use crate::skills;
use crate::update_check::{self, CheckOutcome};

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
/// The header's timer readout — the source's `FontSize="16"
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
/// The AbilityScore/SeasonStrength suffix trailing the name (issue #168).
/// Issue #188: deliberately smaller than `FONT_SIZE_ROW` so the suffix
/// reads as secondary metadata rather than an equal continuation of the
/// name — position is unaffected, since it's still anchored at
/// `name_rect.right_center()`, only the font size argument changes.
const FONT_SIZE_ROW_SUFFIX: f32 = 11.0;

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

/// How long a *transient* `StatusLine::Error` stays up before taking
/// itself down again (PR #197 review). Long enough to read at a glance,
/// short enough that a momentary clipboard blip doesn't outlive the
/// encounter it happened during — see `OverlayApp::status_expires_at` for
/// which banners expire and which are permanent.
const TRANSIENT_STATUS_LINGER: Duration = Duration::from_secs(5);

/// The overlay's eframe app: holds the latest snapshot plus the channel
/// endpoints used to receive updates and send commands.
pub struct OverlayApp {
    snapshot: Snapshot,
    status: StatusLine,
    /// When the current `status` banner stops being drawn — `Some` only
    /// while a *transient* banner is up (PR #197 review). The Share
    /// clipboard failure `ui()` raises is the only one: Windows hands the
    /// clipboard out under a lock, so a write can fail purely because some
    /// other process held it for an instant, and nothing used to clear the
    /// banner afterwards — no timer, no success path, not even a later
    /// Share that worked — so one blip left the red line under the header
    /// for the rest of the session. `None` for `StatusLine::Ok` and for the
    /// permanent capture-init failure `main.rs` seeds through
    /// `with_status`, which must stay up forever precisely because it is
    /// still true on every later frame.
    status_expires_at: Option<Instant>,
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
    /// Issue #83: last-logged `(pixels_per_point, zoom_factor)`, so the
    /// per-frame DPI probe in `ui()` only writes to the log when the value
    /// actually changes rather than every ~10Hz frame.
    last_dpi_probe: Option<(f32, f32)>,
    /// Issue #183: the OS-level mouse-passthrough state last handed to
    /// `egui::ViewportCommand::MousePassthrough`, so `ui()` only sends the
    /// command when the answer actually changes rather than ~10 times a
    /// second. `false` at startup, matching a freshly created window's own
    /// (unset) `WS_EX_TRANSPARENT`. See
    /// `platform::click_through_passthrough_wanted` for what computes the
    /// value and why click-through needs it at all.
    mouse_passthrough: bool,
    /// Issue #96 (PR #98 review): the row-list content bottom edge — top
    /// chrome plus only the populated rows, in window-space points
    /// (`rows_content_bottom_y`) — for the Share screenshot request
    /// currently in flight, or `None` if no request is in flight.
    ///
    /// `handle_screenshot_events` runs at the *start* of `ui()`, but the
    /// `Event::Screenshot` reply to a Share click lands asynchronously, on
    /// some later frame than the click — and `self.snapshot`'s row count
    /// is live combat data that can change on any frame in between (a
    /// player joining or dropping mid-encounter is routine). Reading
    /// whatever row-bottom bound happens to be current when the reply
    /// lands would crop against a row count that no longer matches the
    /// pixels actually captured. Instead, the bound is computed and
    /// stashed here once, at the end of the *same* frame the click
    /// happened on (after `draw_rows` has laid out that frame's rows —
    /// the Share button itself is drawn earlier, in the header), and
    /// `handle_screenshot_events`'s call site `take()`s it when the reply
    /// lands, so the crop always matches the frame that was actually
    /// captured regardless of how the row count drifts afterward.
    ///
    /// Issue #82: that `take()` must only happen on the frame that
    /// actually handles a screenshot event — see
    /// `take_pending_screenshot_bound`'s doc comment for the regression an
    /// unconditional, once-per-frame `take()` caused (the bound was
    /// discarded before the asynchronous reply could ever land, so every
    /// Share click failed).
    pending_screenshot_bound: Option<f32>,
    /// Issue #156: whether a Share screenshot capture is currently in
    /// flight — set the moment `toggle_cluster` fires the
    /// `ViewportCommand::Screenshot` request, cleared the moment the
    /// `Event::Screenshot` reply lands (`screenshot_capture_guard` is the
    /// pure state transition). Read at the top of each frame and threaded
    /// down through `draw_header` into `toggle_cluster` so the toggle
    /// buttons can suppress their hover fill and tooltip on the frame that
    /// actually gets captured, not just the frame the click happened on —
    /// `ViewportCommand::Screenshot` captures "the next frame after" the
    /// one that sends it, and the reply can land any number of frames
    /// after that.
    ///
    /// Not latched forever if the reply never comes: egui-wgpu's
    /// `Painter::paint_and_update_textures` has several early-return paths
    /// (a failed surface recreate, or its `render_state`/`surface_state`
    /// being `None`) that skip `read_screen_rgba` entirely, silently
    /// dropping the queued screenshot with no `Event::Screenshot` ever
    /// pushed. `screenshot_capture_frames_waited` bounds how long this
    /// stays `true` with no landed reply — see `screenshot_capture_timed_
    /// out`'s doc comment.
    screenshot_capturing: bool,
    /// Issue #171: state of a manual "Check for updates" request from the
    /// header dropdown — `Idle` until clicked, `Checking` while
    /// `update_check::check_for_update` runs on its spawned thread, then
    /// `Done` once its result lands over the channel. Lives here (not
    /// local to `draw_header_menu`) so the in-flight state and the last
    /// result both survive the dropdown popup closing and reopening
    /// mid-check, and so `poll_update_check` can drain the channel once
    /// per frame regardless of whether the dropdown happens to be open
    /// that frame. See `UpdateCheckState`'s own doc comment.
    update_check: UpdateCheckState,
    /// Issue #220 (PR #227 review): replies from the one-shot threads
    /// `start_log_export` spawns for the header dropdown's "Export logs"
    /// item. Drained once per frame by `poll_log_export`, unconditionally,
    /// the same shape `poll_update_check` and `poll_history` use — the
    /// dropdown closes on the click, so a reply has no menu state left to
    /// land in and goes to the status banner instead.
    ///
    /// A persistent channel pair (rather than `UpdateCheckState`'s
    /// per-request channel) because nothing disables the menu item while an
    /// export runs: two exports can legitimately be in flight at once, and
    /// a single stored `Receiver` would drop the first one's reply.
    rx_log_export: Receiver<LogExportOutcome>,
    /// The sender half of `rx_log_export`, cloned into each export thread.
    tx_log_export: Sender<LogExportOutcome>,
    /// Issue #156: consecutive frames `screenshot_capturing` has been
    /// held `true` with neither a new request nor a landed reply this
    /// frame — reset to `0` by `advance_screenshot_capture_wait` the
    /// instant either happens, incremented every frame in between. Compared
    /// against `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES` by `screenshot_capture_
    /// timed_out` so a dropped reply can't suppress the toggle cluster's
    /// hover fill and tooltip for the rest of the process.
    screenshot_capture_frames_waited: u32,
    /// `demo_enabled()` cached at construction so `ui()` doesn't re-read the
    /// env var every frame; also lets `ui()` keep demo mode's synthetic
    /// snapshot from being clobbered by the per-frame `rx_snapshot` drain
    /// below (see that call site).
    demo_mode: bool,
    /// Whether `ui()` has already applied the startup click-through/
    /// always-on-top pair that re-applies `Settings::click_through`/
    /// `always_on_top` (issue #167) — `platform::set_click_through` for
    /// the former (issue #167 rehash: no longer a `ViewportCommand`, see
    /// that function's doc comment) and `ViewportCommand::WindowLevel` for
    /// the latter. The `WindowLevel` half needs a live `egui::Context`,
    /// which — same as `icons` above — does not exist yet at
    /// `OverlayApp::new`, so this is set on the first `ui()` call and
    /// never again; every later frame's toggle state instead flows
    /// through `toggle_cluster`'s own click handling and the tray menu's
    /// "Turn off click-through" escape hatch, not this flag.
    startup_toggles_applied: bool,
    /// Per-player skill breakdown windows currently open (issue #16), keyed
    /// by player uid; the value is that window's own sort state plus the
    /// screen position it was placed at and the size it is shown at
    /// (`SkillWindowState`).
    /// Lives on the app rather than in egui memory for the same reason
    /// `window_gesture` does: `ctx.show_viewport_immediate` runs each
    /// child's UI on this same frame and thread, which is precisely what
    /// lets this be an owned field instead of an `Arc<Mutex<..>>`. A
    /// `BTreeMap` so several open windows keep a stable draw order across
    /// frames.
    skill_windows: std::collections::BTreeMap<i64, SkillWindowState>,
    /// Issue #39: the history thread's handle, or `None` when history is
    /// disabled or its database could not be opened — every history control is
    /// then simply absent, and nothing else changes.
    history: Option<history::writer::HistoryHandle>,
    /// Issue #39: replies from the history thread. Drained once per frame by
    /// `poll_history`, regardless of whether the history view is open, so a
    /// reply that lands while it is closed is not dropped — the same reason
    /// `poll_update_check` drains unconditionally (issue #171).
    rx_history: Receiver<history::writer::HistoryEvent>,
    /// The sender half of `rx_history`, cloned into each request.
    tx_history: Sender<history::writer::HistoryEvent>,
    /// Issue #39: which surface is showing. See `OverlayView`.
    view: OverlayView,
}

/// All icon textures the overlay paints, bundled so `OverlayApp` has exactly
/// one lazily-loaded field for them instead of one per icon set (issue #41).
///
/// Every set here is `include_bytes!`-ed at compile time (issue #123), so
/// `Icons::load` has nothing to resolve or warn about — it just decodes and
/// uploads a texture for each set.
struct Icons {
    classes: ClassIcons,
    toolbar: ToolbarIcons,
    glyphs: GlyphIcons,
    // IMAGINE-TAKEDOWN: one of five sites — see
    // `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
    imagines: ImagineIcons,
    /// Per-skill row icons for the breakdown window (issue #192).
    skills: SkillIcons,
}

impl Icons {
    /// Safe to call more than once per process (each call re-decodes and
    /// re-uploads every icon), but nothing does: `ui.rs`'s
    /// `get_or_insert_with` call site only ever calls this on `OverlayApp`'s
    /// first `ui()` frame.
    fn load(ctx: &egui::Context) -> Self {
        Self {
            classes: ClassIcons::load(ctx),
            toolbar: ToolbarIcons::load(ctx),
            glyphs: GlyphIcons::load(ctx),
            imagines: ImagineIcons::load(ctx),
            skills: SkillIcons::load(ctx),
        }
    }
}

/// The overlay only ever renders real data when the game is running on
/// Windows, which makes UI work on the header (issue #91) unverifiable on a
/// dev box — there is no live encounter to populate it with. Setting
/// `SHINRA_DEMO=1` seeds a fixed synthetic encounter instead of "No target",
/// so the issue #88 uidbg harness can capture a populated header without a
/// game session. Opt-in and off by default; see [`demo_enabled_from`] for the
/// exact truthiness rule (same idiom as `main::composition_choice`'s
/// `SHINRA_NO_COMPOSITION`, but with the true/false senses swapped since this
/// one defaults off).
fn demo_enabled() -> bool {
    demo_enabled_from(std::env::var("SHINRA_DEMO").ok().as_deref())
}

/// Pure truthiness check for `SHINRA_DEMO`, split out from [`demo_enabled`]
/// so it is testable without touching the process environment. `1`, `true`,
/// or `on` (case-insensitively) turn demo mode on; everything else —
/// including unset, empty, and any other value — leaves it off.
fn demo_enabled_from(var: Option<&str>) -> bool {
    var.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
}

/// `(skill_id, damage, hits, crit_hits, crit_damage, max_crit)` for one
/// skill in a `DemoRow`'s breakdown (issue #16). This is deliberately more
/// than `bpsr_meter::SkillStats` carries at rest (it has no `crit_damage`
/// field — only a running `max_crit`) because a `SkillRow`'s `avg_crit` and
/// `avg_white` are *means*, and a literal fixture has no per-hit event
/// stream to mean over; carrying `crit_damage` explicitly is the minimum
/// needed for [`demo_skill_rows`] to hand every field off to
/// `bpsr_meter::skill_row_from_stats` — the same formulas the real
/// aggregator uses, rather than inventing a second, looser set of
/// demo-only ones.
type DemoSkill = (i32, i64, u64, u64, i64, i64);

/// `(name, class, damage, crit_pct, lucky_pct, hits, deaths, imagines,
/// imagine_tiers, skills)` for one `DEMO_ROWS` entry. Named so the array
/// below reads as one type, not clippy's `type_complexity` bait.
type DemoRow = (
    &'static str,
    Class,
    i64,
    f32,
    f32,
    u64,
    u32,
    [Option<i32>; 2],
    [Option<i32>; 2],
    &'static [DemoSkill],
);

/// `(name, class, damage, crit_pct, lucky_pct, hits, deaths, imagines,
/// imagine_tiers)` for each demo row, in descending-damage order (issue
/// #148). A realistic
/// dungeon/raid comp — one tank (`Thudd`, blue `ShieldKnight`), one healer
/// (`Fizz`, green `VerdantOracle`), three damage dealers — rather than five
/// DPS rows, per `Class::role()`. The *names* are deliberately fictional,
/// never a real character name — this repo is public, and
/// `CONTRIBUTING.md` tells users not to share other players' names, so a
/// demo capture headed for the README can't republish someone else's
/// (issue #133).
///
/// Crit/lucky pairs are shaped to read as draws from one shared substat
/// budget, clamped to a plausible 5-70% range: `Blorp` and `Fizz` build
/// nearly all-in on one stat (sums ~75-76%), `Zog` splits close to evenly
/// (sum ~73%, and so sits lower on both axes than the all-in rows),
/// and `Glorbaxian` lands in between (sum ~74%). `Thudd` (the tank) pairs
/// a conservative 22% crit with 18% lucky (sum 40%)—intentionally modest
/// to reflect a tank's priority on survivability and mitigation over
/// offensive stats. Hits and deaths are shaped on a real tank-vs-DPS
/// ratio — `Thudd` racks up far more hits (a tank's rotation is
/// faster/lower per hit) and nobody but `Glorbaxian` dies.
///
/// `imagines` is each row's two equipped-Imagine skill ids, demo-only:
/// they're picked from `imagines::imagine_of_skill_id`'s curated table
/// purely so a demo capture shows the column doing real work instead of ten
/// blank placeholder circles. The ids aren't class-locked, so `Thudd`
/// reuses the ids of the DPS row it replaces. The last row keeps one slot
/// empty on purpose, since not every real player fills both. Folded into
/// this tuple (rather than a separate by-index array) so a row and its
/// Imagine pair can never drift apart — reordering `DEMO_ROWS` carries its
/// Imagines along, and there is no second array whose length or order could
/// silently disagree.
///
/// `imagine_tiers` (issues #169/#170) is each slot's demo tier, positionally
/// paired with `imagines`. `Glorbaxian`'s first slot is deliberately pinned
/// at `IMAGINE_MAX_TIER` so the demo capture actually shows the gold ring
/// doing real work, not just the tooltip text.
///
/// `skills` (issue #16) is each row's per-skill breakdown, folded into the
/// same tuple entry for the same reason `imagines` is: a row and its skills
/// can't drift apart structurally, and there is no second by-index array
/// whose length could silently disagree with `DEMO_ROWS`. Each skill's
/// `damage`/`hits` are hand-picked to sum *exactly* to the row's own
/// `damage`/`hits` — an inconsistent breakdown would contradict the row it
/// opens from, which is exactly the class of header/row disagreement bug
/// issue #148 already burned this file on once (see the `demo_snapshot` doc
/// comment below). Skill ids are real ids from the vendored
/// `SkillOverridesNames.json` curated table, picked so `tables::skill_name`
/// resolves every one of them to a real name instead of the `Skill #<id>`
/// fallback — a demo capture with placeholder names would be worthless for
/// eyeballing the breakdown window. Each row gets 4-5 skills in a
/// descending damage/hit split (a "signature" skill carrying roughly a
/// third to two-fifths of the row, tapering off), with per-skill crit hits
/// apportioned from the row's own `crit_pct` and crit damage weighted
/// above the row's white-hit average, so `Max crit >= Avg crit` holds the
/// way a real combat log's would.
const DEMO_ROWS: [DemoRow; 5] = [
    (
        "Blorp",
        Class::Stormblade,
        55_300_000,
        68.0,
        8.0,
        150,
        0,
        [Some(3901), Some(3902)],
        [Some(3), Some(1)],
        &[
            (1605, 21_014_000, 45, 30, 15_760_500, 683_955), // Blazing Ascension
            (1607, 13_272_000, 36, 24, 9_954_000, 540_175),  // Blazing Assault
            (1613, 8_848_000, 30, 20, 6_636_000, 432_340),   // Wildfire Dance
            (1617, 7_189_000, 22, 14, 5_205_828, 484_398),   // Endless Hellfire
            (1623, 4_977_000, 17, 11, 3_649_800, 432_340),   // Great Crimson Lotus
        ],
    ),
    (
        "Glorbaxian",
        Class::FrostMage,
        55_100_000,
        52.0,
        22.0,
        180,
        1,
        [Some(3903), Some(3904)],
        [Some(IMAGINE_MAX_TIER), Some(2)],
        &[
            (1239, 20_938_000, 54, 28, 12_932_294, 601_428), // Meteor Storm
            (1241, 13_224_000, 43, 22, 8_081_333, 478_533),  // Frostbeam
            (1244, 8_816_000, 36, 18, 5_289_600, 383_027),   // Blizzard
            (1258, 7_163_000, 27, 14, 4_424_206, 411_819),   // Icy Bolt
            (1259, 4_959_000, 20, 10, 2_975_400, 387_802),   // Frost Comet
        ],
    ),
    (
        "Zog",
        Class::TwinStriker,
        49_900_000,
        38.0,
        35.0,
        210,
        0,
        [Some(3905), Some(3906)],
        [Some(0), Some(4)],
        &[
            (1714, 18_962_000, 63, 23, 8_781_060, 497_321), // Iaido Slash
            (1715, 11_976_000, 50, 19, 5_736_403, 393_491), // Moonstrike
            (1727, 7_984_000, 42, 15, 3_629_091, 315_521),  // Piercing Slash
            (1728, 6_487_000, 32, 12, 3_072_789, 333_885),  // Ultimate Slash
            (1736, 4_491_000, 23, 8, 1_996_000, 325_350),   // Phantom Slash
        ],
    ),
    (
        "Thudd",
        Class::ShieldKnight,
        17_800_000,
        22.0,
        18.0,
        540,
        0,
        [Some(3907), Some(3908)],
        [Some(1), Some(1)],
        &[
            (1401, 6_764_000, 162, 35, 1_978_329, 74_481), // Windborne Grace - Sweep
            (1406, 4_272_000, 130, 28, 1_246_000, 58_850), // Windborne Grace
            (1411, 2_848_000, 108, 23, 822_226, 47_474),   // Swift Blade
            (1423, 2_314_000, 81, 17, 659_296, 51_417),    // Aegis Gale
            (1932, 1_602_000, 59, 12, 443_631, 49_060),    // Shield Combo
        ],
    ),
    (
        "Fizz",
        Class::VerdantOracle,
        10_300_000,
        10.0,
        65.0,
        90,
        0,
        [Some(3909), None],
        [Some(2), None],
        &[
            (1550, 4_326_000, 32, 3, 581_104, 252_812), // Feral Seed - Seed Meteor
            (1551, 2_781_000, 25, 2, 320_885, 209_575), // Regen Bud: Wild Seed
            (1556, 1_854_000, 18, 1, 150_324, 196_421), // Bloomheal
            (1560, 1_339_000, 15, 1, 129_581, 169_455), // Regen Pulse
        ],
    ),
];

/// Derives one demo row's `SkillRow`s from its `DemoSkill` fixture list
/// (issue #16) via `bpsr_meter::skill_row_from_stats` — the same formulas
/// the real aggregator uses, kept in sync by construction instead of a
/// second copy of the arithmetic living here. Sorted damage-descending
/// like the real snapshot, so the breakdown window's default sort (D9) is
/// a no-op on first paint here too.
fn demo_skill_rows(skills: &[DemoSkill], player_damage: i64, duration_ms: u64) -> Vec<SkillRow> {
    let mut rows: Vec<SkillRow> = skills
        .iter()
        .map(
            |&(skill_id, damage, hits, crit_hits, crit_damage, max_crit)| {
                let stats = SkillStats {
                    total_damage: damage,
                    hits,
                    crit_hits,
                    crit_damage,
                    max_crit,
                    ..Default::default()
                };
                skill_row_from_stats(skill_id, &stats, player_damage, duration_ms)
            },
        )
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.damage));
    rows
}

/// The synthetic snapshot `demo_enabled` seeds the overlay with. The header's
/// `total_damage`/`total_dps` are derived from `DEMO_ROWS` rather than a
/// separate literal (issue #148), so the two can never disagree the way they
/// used to (the old header borrowed a duration/DPS/total-damage figure from
/// `docs/reference/new-shinra-ex.webp` independent of the row data, two
/// orders of magnitude off from what the rows actually summed to). Boss and
/// scene are a real BPSR pull: `Purge! Field of Forgotten Illusions`'s final
/// boss, `Paradox-Calamity Remnant - Final` (`tables.rs`), a fight that runs
/// well within this snapshot's 159s duration in practice.
fn demo_snapshot() -> Snapshot {
    let row_damage_sum: i64 = DEMO_ROWS.iter().map(|(_, _, dmg, ..)| dmg).sum();
    let duration_ms = 159_000u64;
    let rows = DEMO_ROWS
        .iter()
        .enumerate()
        .map(
            |(
                i,
                &(
                    name,
                    class,
                    damage,
                    crit_pct,
                    lucky_pct,
                    hits,
                    deaths,
                    imagine_ids,
                    imagine_tiers,
                    demo_skills,
                ),
            )| {
                PlayerRow {
                    uid: i as i64 + 1,
                    name: name.to_string(),
                    class: Some(class),
                    ability_score: None,
                    season_strength: None,
                    imagines: imagine_ids,
                    imagine_tiers,
                    damage,
                    dps: damage as f64 / (duration_ms as f64 / 1000.0),
                    share_pct: damage as f32 / row_damage_sum as f32 * 100.0,
                    crit_pct,
                    lucky_pct,
                    hits,
                    deaths,
                    skills: demo_skill_rows(demo_skills, damage, duration_ms),
                }
            },
        )
        .collect();
    Snapshot {
        duration_ms,
        total_damage: row_damage_sum,
        total_dps: row_damage_sum as f64 / (duration_ms as f64 / 1000.0),
        rows,
        encounter: EncounterInfo {
            boss_monster_id: Some(103_309),
            boss_name: Some("Paradox-Calamity Remnant - Final"),
            is_boss: true,
            scene_id: Some(13_023),
            scene_name: Some("Purge! Field of Forgotten Illusions"),
            scene_boss_name: None,
            multi_boss_scene: false,
        },
    }
}

/// The snapshot `OverlayApp::new` seeds itself with: `demo_snapshot()` when
/// demo mode is on, otherwise the ordinary empty "No target" state. Split out
/// from `new` itself — which otherwise cannot be unit-tested for this without
/// mutating the process's `SHINRA_DEMO` env var (racy across the crate's
/// parallel test threads, and `unsafe` as of the 2024 edition) — so the
/// seeding decision is directly testable, the same way `demo_enabled_from`
/// is split out from `demo_enabled`.
fn initial_snapshot(demo_mode: bool) -> Snapshot {
    if demo_mode {
        demo_snapshot()
    } else {
        Snapshot {
            duration_ms: 0,
            total_damage: 0,
            total_dps: 0.0,
            rows: Vec::new(),
            encounter: EncounterInfo::default(),
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
        // Issue #39: `None` when history is disabled in settings.json or its
        // database could not be opened (`main.rs` already logged why) —
        // every history control is then simply absent.
        history: Option<history::writer::HistoryHandle>,
    ) -> Self {
        // Demo seed (see `demo_enabled`/`demo_snapshot` above). Cached once
        // here rather than re-called, so `ui()` below can reuse the same
        // answer every frame instead of re-reading the env var.
        let demo_mode = demo_enabled();
        let snapshot = initial_snapshot(demo_mode);
        let (tx_history, rx_history) = crossbeam_channel::unbounded();
        let (tx_log_export, rx_log_export) = crossbeam_channel::unbounded();
        Self {
            snapshot,
            status: StatusLine::Ok,
            status_expires_at: None,
            settings,
            rx_snapshot,
            tx_command,
            tx_settings,
            icons: None,
            window_gesture: WindowGesture::default(),
            mouse_passthrough: false,
            last_dpi_probe: None,
            pending_screenshot_bound: None,
            screenshot_capturing: false,
            update_check: UpdateCheckState::Idle,
            rx_log_export,
            tx_log_export,
            screenshot_capture_frames_waited: 0,
            demo_mode,
            startup_toggles_applied: false,
            skill_windows: std::collections::BTreeMap::new(),
            history,
            rx_history,
            tx_history,
            view: OverlayView::Live,
        }
    }

    /// Seeds the *permanent* status banner from `main.rs` — today only the
    /// capture-init failure, which no later frame can undo. Deliberately
    /// leaves `status_expires_at` at `None`, and that is the whole of what
    /// exempts this banner from the timeout and from a successful Share
    /// clearing it (PR #197 review); see `status_expires_at`.
    pub fn with_status(mut self, status: StatusLine) -> Self {
        self.status = status;
        self
    }

    /// Raises a transient error banner (PR #197 review). It is the expiry
    /// this stamps alongside the message — not the `StatusLine::Error`
    /// itself — that marks the banner as one `clear_transient_status` and
    /// `expire_transient_status` are allowed to take back down.
    fn raise_transient_status(&mut self, message: String, now: Instant) {
        self.status = StatusLine::Error(message);
        self.status_expires_at = Some(now + TRANSIENT_STATUS_LINGER);
    }

    /// Takes a transient banner down at once — what a Share that *worked*
    /// does, so a failure followed by a success stops claiming the copy
    /// failed. A permanent banner carries no expiry, so this leaves it
    /// exactly where it is.
    fn clear_transient_status(&mut self) {
        if self.status_expires_at.take().is_some() {
            self.status = StatusLine::Ok;
        }
    }

    /// The same clear on a timer rather than on a success, for the failure
    /// no later Share ever follows. Called once per frame from `ui()`,
    /// which is tick enough: the app repaints unconditionally at ~10Hz
    /// (`ctx.request_repaint_after` at the end of `ui()`).
    fn expire_transient_status(&mut self, now: Instant) {
        if self.status_expires_at.is_some_and(|at| now >= at) {
            self.clear_transient_status();
        }
    }

    /// Drains `rx_snapshot`, keeping only the most recent snapshot. Demo
    /// mode's snapshot is synthetic, not a stand-in for "no live data yet" —
    /// draining here would replace it with the pipeline's real (empty,
    /// game-not-running) snapshots every frame, which is exactly the bug
    /// this skip avoids. Split out of `ui()` so the guard is unit-testable
    /// without an `egui::Context`.
    fn drain_snapshots(&mut self) {
        if !self.demo_mode {
            for snap in self.rx_snapshot.try_iter() {
                self.snapshot = snap;
            }
        }
    }

    /// Issue #171: picks up the manual update-check thread's result, if one
    /// is in flight and has landed — the counterpart to `drain_snapshots`,
    /// called once per frame from `ui()` so a reply that arrives while the
    /// header dropdown happens to be closed is still there the moment it's
    /// reopened, rather than dropped or leaving the dropdown stuck showing
    /// "Checking…" forever.
    fn poll_update_check(&mut self) {
        let landed = match &self.update_check {
            UpdateCheckState::Checking { rx } => match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                // Still in flight — keep rendering "Checking…".
                Err(TryRecvError::Empty) => None,
                // The sender is gone without ever having sent: the
                // update-check thread died (a panic in the unsafe WinHTTP
                // FFI, say) instead of reporting. Collapsing this into
                // `Empty` — which `try_recv().ok()` used to do — left the
                // state `Checking` forever, and with it the "Check for
                // updates" button disabled, so the user could never retry
                // short of restarting the app. Resolve it as a failure
                // instead, which the dropdown renders and which leaves the
                // button clickable again.
                Err(TryRecvError::Disconnected) => Some(Err(
                    "the update-check thread stopped without reporting a result".to_string(),
                )),
            },
            _ => None,
        };
        if let Some(outcome) = landed {
            self.update_check = UpdateCheckState::Done(outcome);
        }
    }

    /// Issue #220 (PR #227 review): picks up whatever the "Export logs"
    /// threads have finished. Like `poll_update_check`, drained once per
    /// frame whether or not the dropdown is open — the item closes the
    /// dropdown on click, so by the time a multi-megabyte copy finishes
    /// there is no menu left to report through.
    ///
    /// A failure lands on the panel's existing transient error banner (the
    /// same one the Share clipboard failure uses) as well as the log: the
    /// log-only reporting this used to do told a user whose export silently
    /// produced nothing exactly nothing, in the one situation where they
    /// are already trying to hand over a log. A success is logged only —
    /// `StatusLine` has no non-error state to say it with, and the file
    /// appearing where the user just chose to put it is its own feedback.
    fn poll_log_export(&mut self, now: Instant) {
        // Collected before the loop rather than iterated in place: the
        // failure arm below needs `&mut self`, which a live borrow of
        // `self.rx_log_export` would rule out.
        let landed: Vec<LogExportOutcome> = self.rx_log_export.try_iter().collect();
        for outcome in landed {
            match outcome {
                Ok(dest) => log::info!("exported logs to {}", dest.display()),
                Err((dest, err)) => {
                    log::warn!("failed to export logs to {}: {err}", dest.display());
                    self.raise_transient_status(format!("Export logs failed: {err}"), now);
                }
            }
        }
    }
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_snapshots();
        self.poll_update_check();
        // Issue #39: drained unconditionally, regardless of whether the
        // history view is open — see `poll_history`'s doc comment.
        self.poll_history();

        let ctx = ui.ctx().clone();
        apply_theme(&ctx);

        // Issue #167: re-applies `Settings::click_through`/`always_on_top`
        // exactly once, on the first frame a live `egui::Context` exists
        // (same one-shot shape as the icon load right below). `viewport()`
        // already bakes `always_on_top`'s hardcoded default into the window
        // builder — this is only needed to *correct* that for a returning
        // user whose last session turned either toggle away from its
        // default, and to turn click-through on at all, since nothing in
        // `viewport()` sets it up front.
        if !self.startup_toggles_applied {
            self.startup_toggles_applied = true;
            crate::platform::set_click_through(self.settings.click_through);
            let level = if self.settings.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        }

        // Issue #167 rehash: syncs `Settings::click_through` (and the
        // toggle-cluster button's displayed state) with a "Turn off
        // click-through" request raised by the tray menu — the escape
        // hatch that replaced the `Ctrl+Alt+P` hotkey, which turned out
        // not to work (egui only sees key events while the overlay holds
        // keyboard focus, and click-through's whole point is clicking the
        // game *behind* the overlay, which takes focus away). The actual
        // OS-level passthrough is already off by the time this runs —
        // `platform::request_click_through_off` clears it directly, for
        // immediacy, the instant the tray command fires — so this only
        // catches Settings and the button's paint up to what the window
        // is already doing. See `click_through_after_tray_request`'s doc
        // comment for the pure decision.
        let tray_click_through_off = crate::platform::take_tray_click_through_off_request();
        let click_through =
            click_through_after_tray_request(self.settings.click_through, tray_click_through_off);
        if click_through != self.settings.click_through {
            self.settings.click_through = click_through;
            crate::platform::set_click_through(click_through);
            let _ = self.tx_settings.send(self.settings.clone());
        }

        // Issue #83: a prior investigation of "player rows render shorter/
        // more compact than the reference" found no code-level defect —
        // `ROW_HEIGHT`, `FONT_SIZE_ROW` and the row-painting math all check
        // out — but couldn't rule out a runtime DPI effect on the
        // reporter's real Windows box; no screenshot was obtainable in this
        // environment to compare directly. Logged only when the value
        // changes, so a stuck-open overlay repainting at ~10Hz doesn't spam
        // the log file. A suspicious reading looks like `pixels_per_point`
        // not matching the display's actual OS scaling (e.g. staying `1.0`
        // on a 150%-scaled monitor) or a `zoom_factor` other than `1.0` —
        // either would shrink everything the overlay paints, rows included,
        // without needing a defect in this file at all.
        let dpi_probe = (ctx.pixels_per_point(), ctx.zoom_factor());
        if self.last_dpi_probe != Some(dpi_probe) {
            log::debug!(
                "issue #83 probe: pixels_per_point={:.3} zoom_factor={:.3}",
                dpi_probe.0,
                dpi_probe.1
            );
            self.last_dpi_probe = Some(dpi_probe);
        }

        // Picks up the Share button's screenshot reply, if this frame has
        // one (issue #82: `toggle_cluster` fired the request one or more
        // frames ago via `ViewportCommand::Screenshot`; the round trip is
        // asynchronous, so it can land on any later frame). Checked every
        // frame, unconditionally — a screenshot can be requested at any
        // point in the header, which is always painted.
        // Issue #96 (PR #98 review): crop to the header chrome + populated
        // rows before the clipboard write, using the bound captured at
        // *request* time (`pending_screenshot_bound`) rather than whatever
        // bound is current on the frame the reply happens to land on — see
        // `pending_screenshot_bound`'s doc comment.
        // Issue #82 (async round-trip regression, see
        // `take_pending_screenshot_bound`'s doc comment): the pending bound
        // must only be consumed on the frame that actually handles a
        // screenshot event, not unconditionally every frame — an
        // unconditional `take()` here discarded the bound before the
        // asynchronous reply could ever land, which is why the Share
        // button failed on every single click. The sequencing itself lives
        // in `handle_share_screenshot`, extracted out for the same
        // testability reason `handle_screenshot_events` is: see its doc
        // comment.
        // Issue #156: whether this frame is the one that handles the Share
        // round trip's `Event::Screenshot` reply — fed into `screenshot_
        // capture_guard` below alongside whether this same frame's Share
        // click fires a *new* request, to decide what `self.screenshot_
        // capturing` must hold for the next frame's `draw_header` call.
        let mut screenshot_event_landed = false;
        // Issue #183: whatever the clipboard write reported, kept in a local
        // because the closure below already holds a `&mut` borrow of
        // `self.pending_screenshot_bound`; raised as the status banner right
        // after it.
        let mut share_error: Option<String> = None;
        handle_share_screenshot(&ctx, &mut self.pending_screenshot_bound, |image| {
            screenshot_event_landed = true;
            // Issue #183: flattened first — a premultiplied-alpha capture of
            // a *transparent* overlay pastes as a near-invisible image,
            // which is exactly what "the Copy button does nothing" looked
            // like from the outside. See `flatten_screenshot_alpha`.
            let opaque = flatten_screenshot_alpha(&image);
            if let Err(err) = crate::platform::write_clipboard_image(&opaque) {
                log::warn!("the Share screenshot could not be copied: {err}");
                share_error = Some(err);
            }
        });
        // Issue #183: surfaced on the panel's existing one-line error banner
        // (`StatusLine::Error`, drawn just under the header) rather than
        // swallowed into a log nobody reads, so a Share click that failed no
        // longer looks identical to one that worked.
        // PR #197 review: raised *transiently*. A locked clipboard is a
        // momentary condition, so the banner clears itself after
        // `TRANSIENT_STATUS_LINGER` and is taken down early by the next
        // Share round trip that lands without an error — it used to be the
        // only write to `status` after construction, with no reader that
        // ever reset it, so a single blip stuck for the session.
        let now = Instant::now();
        if let Some(err) = share_error {
            self.raise_transient_status(format!("Copy screenshot failed: {err}"), now);
        } else if screenshot_event_landed {
            self.clear_transient_status();
        }
        // Issue #220 (PR #227 review): drained here, alongside the Share
        // banner handling just above, because a failed export raises the
        // same transient banner and so needs the same `now`.
        self.poll_log_export(now);
        self.expire_transient_status(now);
        // Issue #183: reconcile the OS-level mouse passthrough with what
        // click-through wants *this* frame — see `platform::click_through_
        // passthrough_wanted` for why `window_proc`'s `WM_NCHITTEST`
        // carve-out alone could never hand a click to the game underneath,
        // and how the toggle button stays reachable anyway. Sent only when
        // the answer changes: `MousePassthrough` is a real window-style
        // write, not something worth queueing ~10 times a second for a value
        // that almost never moves.
        let passthrough = crate::platform::click_through_passthrough_wanted();
        if passthrough != self.mouse_passthrough {
            self.mouse_passthrough = passthrough;
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(passthrough));
        }
        // The value `screenshot_capturing` held entering this frame — read
        // once, before `draw_header` runs, so the toggle cluster's paint
        // decision for *this* frame is based on whatever the previous
        // frame's request/reply activity left it as (see `screenshot_
        // capture_guard`'s doc comment for why the request frame itself
        // can't be the one this suppresses).
        let capturing = self.screenshot_capturing;

        // Loaded once, lazily: the `egui::Context` above isn't available yet
        // at `OverlayApp::new`, so the first frame is what actually uploads
        // the icon textures (issues #9, #41); every later frame reuses them.
        let icons = self.icons.get_or_insert_with(|| Icons::load(&ctx));

        // Issue #16 (D1): set by `draw_rows` (via `draw_row`) when a row is
        // right-clicked this frame; consumed below, after this frame's root
        // window rect is known, to open (or re-show) that player's
        // breakdown window.
        let mut opened_skill_uid: Option<i64> = None;
        // Issue #39: the open historical fight — its id, header text and
        // rebuilt `Snapshot` — cloned once per frame *before* the panel body
        // — the two short strings and the ~10-row snapshot are cheap next
        // to a per-frame egui repaint, and cloning them here (rather than
        // borrowing `self.view` for the rest of the frame) is what lets the
        // panel closure below still take `&mut self.settings` for
        // `draw_header` without the borrow checker seeing that as aliasing
        // the same historical data. `None` in the `Live` case, and whenever
        // the history view is open but nothing has been loaded yet, costs
        // no clone at all.
        let history_open: Option<OpenEncounter> = match &self.view {
            OverlayView::History(state) => state.open.clone(),
            OverlayView::Live => None,
        };
        // Issue #219: whether Share may fire this frame — see
        // `share_active_for_view`'s doc comment. Read here (rather than
        // inline in the `draw_header` call below) for the same borrow-
        // ordering reason as `history_open` just above: a plain `&self.view`
        // read, done before the panel closure's mutable borrows begin.
        let share_active = share_active_for_view(&self.view);
        // Issue #39: set by `draw_header_menu`'s "History" item, inside the
        // panel closure below; read back out here, *after* that closure has
        // returned, because acting on it means calling `self.open_history()`
        // — a method that needs the whole `&mut self` free, which it isn't
        // while `icons` (borrowed from `self.icons` above) is still alive
        // for the closure's `draw_header`/`draw_rows` calls.
        let mut open_history_clicked = false;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    // Issue #166: the opacity slider (in the header's
                    // Columns/settings dropdown, `draw_header_menu`) scales
                    // the background fill and border chrome only — row
                    // text, icons, and stat-pill colors are untouched.
                    // That is also the recovery path for issue #182's 0%
                    // floor: with the backdrop gone the glyphs still paint,
                    // so the header stays draggable and the gear stays
                    // clickable (see `Settings::OPACITY_MIN`).
                    // `Color32::gamma_multiply` does the premultiplied-alpha
                    // scaling correctly (see its doc comment on `Color32`
                    // storing channels premultiplied), so there is no need
                    // for a hand-rolled equivalent here.
                    .fill(PANEL_FILL.gamma_multiply(self.settings.opacity))
                    .stroke(egui::Stroke::new(
                        PANEL_BORDER_WIDTH,
                        PANEL_BORDER_COLOR.gamma_multiply(self.settings.opacity),
                    ))
                    .corner_radius(egui::CornerRadius::same(PANEL_CORNER_RADIUS)),
            )
            .show(ui, |ui| {
                // First, so the header buttons drawn afterwards stay on top of
                // the corner zones they overlap.
                draw_resize_handles(ui, &ctx, &mut self.window_gesture, "root");
                // Issue #39: what the header and rows paint this frame — the
                // live snapshot, or the open historical one (`history_open`,
                // cloned above before this closure existed). Computed here,
                // once, so both `draw_header` below and the `draw_rows`/
                // `draw_history` branch further down see the same value.
                let frame_snapshot = history_open
                    .as_ref()
                    .map(|open| &open.snapshot)
                    .unwrap_or(&self.snapshot);
                let header_history = history_open.as_ref().map(|open| HistoryHeader {
                    title: &open.title,
                    subtitle: open.subtitle.as_deref(),
                });
                // Issue #96 (PR #98 review): whether the Share button fired
                // a screenshot request this frame — if so, the row bound
                // this same frame computes below is stashed into
                // `pending_screenshot_bound` for whenever the async reply
                // lands, instead of leaving that field for the crop to read
                // fresh (and possibly stale) at reply time.
                let screenshot_requested = draw_header(
                    ui,
                    &ctx,
                    frame_snapshot,
                    &self.tx_command,
                    SettingsHandle {
                        settings: &mut self.settings,
                        tx_settings: &self.tx_settings,
                    },
                    icons,
                    &mut self.window_gesture,
                    capturing,
                    share_active,
                    &mut self.update_check,
                    &self.tx_log_export,
                    self.history.is_some(),
                    &mut open_history_clicked,
                    header_history,
                );
                // Issue #156: whether this frame's wait for the reply has
                // gone on long enough that it's never coming — computed
                // before the guard call below so a timeout is fed into
                // `screenshot_capture_guard` as `event_landed`, the exact
                // same pure transition that clears the guard on a real
                // reply, rather than a second clearing path to keep in
                // sync with it. Only meaningful while `capturing` is
                // actually true; see `screenshot_capture_timed_out`'s doc
                // comment for why the reply can be silently dropped and
                // never arrive.
                let screenshot_timed_out = capturing
                    && screenshot_capture_timed_out(self.screenshot_capture_frames_waited);
                // Issue #156: fold this frame's request/reply activity into
                // the guard so the *next* frame's `draw_header` call —
                // which, per `ViewportCommand::Screenshot`'s own doc
                // comment, is the one that actually gets captured when
                // `screenshot_requested` is true here — reads a guard
                // that's already set.
                self.screenshot_capturing = screenshot_capture_guard(
                    capturing,
                    screenshot_requested,
                    screenshot_event_landed || screenshot_timed_out,
                );
                self.screenshot_capture_frames_waited = advance_screenshot_capture_wait(
                    self.screenshot_capture_frames_waited,
                    screenshot_requested,
                    screenshot_event_landed || screenshot_timed_out,
                );
                // After the header, so a gesture that started this frame is
                // already anchored — and, being outside it, it is the one
                // place a gesture can end no matter which zone began it.
                drive_window_gesture(&ctx, &mut self.window_gesture, MIN_INNER_SIZE);

                if let StatusLine::Error(msg) = &self.status {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), msg.as_str());
                }

                ui.separator();
                // Issue #96: captured before `draw_rows` consumes the space —
                // `rows_top` is where the scroll area starts,
                // `rows_area_height` is all of what's left in the panel for
                // it, matching `draw_rows`'s own `auto_shrink([false,
                // false])`. Correct as-is for `OverlayView::Live`, which
                // draws nothing between here and `draw_rows`. Issue #219:
                // `OverlayView::History` is different — `draw_history` below
                // paints its own nav bar and a second separator before an
                // open encounter's rows, so that branch overwrites both with
                // the post-chrome values `draw_history` itself measures and
                // returns, rather than leaving these stale ones in place.
                let mut rows_top = ui.cursor().top();
                let mut rows_area_height = ui.available_height();
                // Issue #39: set by the history bar's "← Live" button;
                // checked right after, once the `&mut self.view` borrow the
                // match below needed has ended.
                let mut back_to_live = false;
                match &mut self.view {
                    OverlayView::Live => {
                        draw_rows(
                            ui,
                            &self.snapshot,
                            &self.settings,
                            icons,
                            &mut opened_skill_uid,
                        );
                    }
                    OverlayView::History(state) => {
                        (rows_top, rows_area_height) = draw_history(
                            ui,
                            state,
                            &self.settings,
                            icons,
                            self.history.as_ref(),
                            &self.tx_history,
                            &mut back_to_live,
                            &mut opened_skill_uid,
                        );
                    }
                }
                // PR #225 review of issue #219: resolved through one
                // function, rather than reading `self.view` for the row
                // count and then reassigning it inline here, so the "read
                // before reset" ordering can't drift apart under a future
                // edit — see `resolve_screenshot_row_count`'s doc comment
                // for why the order matters.
                let row_count = resolve_screenshot_row_count(
                    &mut self.view,
                    back_to_live,
                    self.snapshot.rows.len(),
                );
                if screenshot_requested {
                    self.pending_screenshot_bound = Some(rows_content_bottom_y(
                        rows_top,
                        row_count,
                        ROW_HEIGHT,
                        rows_area_height,
                    ));
                }
            });

        // Issue #39: acted on here, not inside the panel closure above —
        // `open_history` needs the whole `&mut self`, which isn't free
        // until `icons`'s borrow of `self.icons` (alive for that closure's
        // `draw_header`/`draw_rows` calls) has ended.
        if open_history_clicked {
            self.open_history();
        }

        // Read once and share with both trackers rather than each calling
        // `ctx.input` separately — also what lets `minimized` be threaded
        // through both as the exact same value for the same frame.
        let (outer_rect, inner_rect, minimized) = ctx.input(|i| {
            let viewport = i.viewport();
            (viewport.outer_rect, viewport.inner_rect, viewport.minimized)
        });
        track_window_position(outer_rect, minimized, &mut self.settings, &self.tx_settings);
        track_window_size(inner_rect, minimized, &mut self.settings, &self.tx_settings);

        // Issue #16 (D1/D3): a right-click on a row this frame opens (or
        // re-shows — never re-closes, see `open_skill_window`) that
        // player's breakdown window. Placement is computed once here, from
        // this frame's already-read root window rect, rather than inside
        // the draw loop below — recomputing `skills::place_window` every
        // frame would fight a user actively dragging the breakdown window,
        // snapping it back to its dock point on the very next repaint.
        if let Some(uid) = opened_skill_uid {
            let monitor = ctx.input(|i| i.viewport().monitor_size);
            let main_outer = outer_rect
                .or(inner_rect)
                .unwrap_or(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::ZERO,
                ));
            // Issue #216 (PR #221 review): the gesture can only have come
            // from whichever surface the panel drew this frame — `draw_rows`
            // in the Live view, `draw_history`'s open-encounter branch in
            // the History one — so `history_open` is exactly which fight the
            // user right-clicked in, and the window carries that from here
            // on instead of re-deciding it per frame.
            let source = history_open
                .as_ref()
                .map_or(SkillWindowSource::Live, |open| {
                    SkillWindowSource::History(open.id)
                });
            let already_open =
                open_skill_window(&mut self.skill_windows, uid, source, || SkillWindowState {
                    sort: skills::SkillSort::default(),
                    pos: skills::place_window(main_outer, monitor, SKILL_WINDOW_SIZE),
                    size: SKILL_WINDOW_SIZE,
                    source,
                    gesture: WindowGesture::default(),
                });
            // Issue #189: the window is no longer always-on-top, so an
            // already-open one can be sitting behind a fullscreen game with
            // no taskbar entry to click. Re-right-clicking the row is the
            // only path back to it, and that path only exists if the
            // gesture raises the viewport — the map is deliberately left
            // untouched (see `open_skill_window`), so this command is the
            // entire visible effect of a second right-click.
            if already_open {
                ctx.send_viewport_cmd_to(skill_viewport_id(uid), egui::ViewportCommand::Focus);
            }
        }

        // Issue #16 (D2): one immediate child viewport per open breakdown
        // window. `show_viewport_immediate` runs the child's UI on this
        // same frame/thread — exactly what lets `skill_windows`' state
        // live as a plain field instead of behind an `Arc<Mutex<..>>` —
        // and the app already repaints at ~10Hz (`request_repaint_after`
        // below), so the extra viewport per open player costs nothing.
        // Re-borrowed here rather than reusing the binding above: that one's
        // borrow of `self.icons` has to end before `open_history` can take
        // `&mut self`, so the child-viewport loop below takes a fresh one.
        let icons = self.icons.as_ref().expect("loaded on the first frame");
        let opacity = self.settings.opacity;
        let mut closed_skill_windows: Vec<i64> = Vec::new();
        // Issue #216: every open window resolves against the fight it was
        // opened from (`SkillWindowState::source`), not against whichever
        // surface is on screen — so a live window keeps its live numbers
        // while the user browses History, and a window opened from a past
        // fight keeps that fight's. `history_open` (built above, before the
        // panel closure) is only *offered* here: it is used by a window
        // whose source names this same encounter id, and by no other.
        let history_rows_for_skill_windows = history_open
            .as_ref()
            .map(|open| (open.id, open.snapshot.rows.as_slice()));
        for (row, uid) in skill_windows_to_draw(
            &self.skill_windows,
            &self.snapshot.rows,
            history_rows_for_skill_windows,
        ) {
            let state = self
                .skill_windows
                .get_mut(&uid)
                .expect("uid came from skill_windows.keys()");
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("{} — skills", row.name))
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(true)
                .with_taskbar(false)
                .with_active(false)
                .with_inner_size(state.size)
                .with_min_inner_size(SKILL_WINDOW_MIN_SIZE)
                .with_position(state.pos);
            // None of `platform::disable_aero_snap`/`install_snap_blocker`/
            // `set_click_through`/`clamp_window_to_visible_area` applies to
            // this child viewport — `platform.rs` caches only the *root*
            // window's HWND, once, from `CreationContext`, and never looks
            // a window up by title or class, so it cannot see this window
            // at all. This is deliberately not click-through (D2).
            let close_requested =
                ctx.show_viewport_immediate(skill_viewport_id(uid), builder, |ui, _class| {
                    let x_clicked = draw_skill_window(
                        ui,
                        row,
                        &mut state.sort,
                        state.source,
                        icons,
                        opacity,
                        &mut state.gesture,
                    );
                    // Issue #181: read from inside the child callback, for
                    // the same reason the close-request check below is —
                    // this is where `ctx.input` reflects the *child*
                    // viewport rather than the root window.
                    track_skill_window_size(
                        &mut state.size,
                        ui.ctx().input(|i| i.viewport().inner_rect),
                    );
                    // Belt-and-braces (D2): an OS-level close (Alt+F4, task
                    // manager, …) must drop this uid exactly like the
                    // in-window `X` does, so the window can never be
                    // orphaned open with no way left to close it.
                    x_clicked || ui.ctx().input(|i| i.viewport().close_requested())
                });
            if close_requested {
                closed_skill_windows.push(uid);
            }
        }
        for uid in closed_skill_windows {
            close_skill_window(&mut self.skill_windows, uid);
        }

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
/// somewhere the user cannot reach it. `outer_rect` and `minimized` are read
/// once per frame by the caller (`OverlayApp::update`) and shared with
/// `track_window_size`, rather than each tracker re-reading `ctx.input`
/// itself.
fn track_window_position(
    outer_rect: Option<egui::Rect>,
    minimized: Option<bool>,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
) {
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

/// Tracks the window's inner (content) size and persists it via the same
/// settings-writer path position uses (issue #134). `inner_rect` is reported
/// on every single frame — including every frame of a resize gesture — so
/// `Settings::with_window_size_if_changed` gates the send on an actual
/// change, the same way `track_window_position` gates on an actual move.
///
/// A minimized window is skipped entirely: some platforms report a zeroed
/// or otherwise meaningless inner size while minimized, which would
/// otherwise be persisted and reopen the overlay unusably small. `inner_rect`
/// and `minimized` are read once per frame by the caller
/// (`OverlayApp::update`) and shared with `track_window_position`, rather
/// than each tracker re-reading `ctx.input` itself.
fn track_window_size(
    inner_rect: Option<egui::Rect>,
    minimized: Option<bool>,
    settings: &mut Settings,
    tx_settings: &Sender<Settings>,
) {
    if minimized == Some(true) {
        return;
    }
    let Some(rect) = inner_rect else {
        return;
    };
    if !is_plausible_size(rect.size()) {
        return;
    }
    if let Some(updated) = settings.with_window_size_if_changed([rect.width(), rect.height()]) {
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

/// Whether a reported inner size is worth persisting at all — belt and
/// braces behind `track_window_size`'s minimized guard, in case a platform
/// reports a zeroed or otherwise meaningless inner size for a frame before
/// the `minimized` flag catches up, mirroring `is_plausible_position`. The
/// bounds are the same ones `sanitize_window_size` enforces when a
/// persisted size is later restored, so nothing rejected here would have
/// survived a restart anyway.
fn is_plausible_size(size: egui::Vec2) -> bool {
    size.x.is_finite()
        && size.y.is_finite()
        && size.x >= MIN_INNER_SIZE.x
        && size.y >= MIN_INNER_SIZE.y
        && size.x <= MAX_INNER_SIZE_DIMENSION
        && size.y <= MAX_INNER_SIZE_DIMENSION
}

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

/// Returns whether the Share button (in the toggle cluster painted at the
/// end of this function) fired a screenshot request this frame — issue #96
/// (PR #98 review): the caller uses this to know whether to stash this same
/// frame's row-bottom bound into `OverlayApp::pending_screenshot_bound`.
// Issue #156's new `capturing` parameter pushes this to 8 genuinely
// independent dependencies (egui plumbing, the snapshot, the command
// channel, the settings handle, icons, the drag gesture, and now the
// screenshot guard) — `settings` already bundles two of what would
// otherwise be separate parameters for the same reason. One more scalar
// flag doesn't earn a second bundling struct of its own.
#[allow(clippy::too_many_arguments)]
fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    gesture: &mut WindowGesture,
    // Issue #156: whether a Share screenshot capture is currently in
    // flight (the request was fired and its `Event::Screenshot` reply has
    // not landed yet) — threaded down to `toggle_cluster` so the toggle
    // buttons can suppress their hover fill and tooltip on the frame that
    // actually gets captured. See `screenshot_capture_guard`'s doc comment
    // for why this can't just be "the frame the click happened on".
    capturing: bool,
    // Issue #219: whether the Share button may fire a screenshot capture
    // this frame — true on `OverlayView::Live` and on `OverlayView::History`
    // once a specific historical encounter is open, false on the bare
    // history list. Threaded down to `toggle_cluster`, which gates both the
    // button's `Sense` (via `toggle_button`'s `enabled`) and its glyph tint
    // (`toggle_state_tint`) on it — the same disabled treatment the History
    // button already wears when history is unavailable. Computed by
    // `share_active_for_view` in `OverlayApp::ui`, the only place that can
    // see `self.view`.
    share_active: bool,
    // Issue #171: the manual "Check for updates" header-menu item's
    // in-flight/last-result state — threaded through to `draw_header_menu`,
    // the only place that reads or mutates it.
    update_check: &mut UpdateCheckState,
    // Issue #220: where the "Export logs" item's spawned copy thread
    // reports back to — threaded through to `draw_header_menu`, the only
    // place that clones it.
    tx_log_export: &Sender<LogExportOutcome>,
    // Issue #39: whether the history thread exists at all — threaded
    // through to `toggle_cluster` (issue #186 moved the History control
    // there from `draw_header_menu`) so its History button can be disabled
    // (rather than absent) when history is off in settings.json or its
    // database could not be opened.
    has_history: bool,
    // Issue #39: set by the toggle cluster's History button (issue #186);
    // read by `OverlayApp::ui` to switch `self.view` into
    // `OverlayView::History`.
    open_history: &mut bool,
    // Issue #39: when `Some`, the two header text lines name a *historical*
    // fight rather than the live encounter (spec DECISION D7). Everything
    // else about the header — timer, DPS readout, toggle cluster, dropdown —
    // is identical in both modes.
    history: Option<HistoryHeader<'_>>,
) -> bool {
    let (title, subtitle) = header_text(snapshot, history.as_ref());
    // The header band's height budget — also what `draw_header_wash` and the
    // paint clips below size themselves against, so the whole band is one
    // number derived once rather than several that could drift apart.
    let band_height = header_band_height(ui.spacing().interact_size.y);

    // The panel's own full-width rect, captured before the band below
    // narrows it to the drag band's height — the background wash (issue #59,
    // #62, #81, #91) is anchored off the panel's left/top/right edges and
    // sized to the whole drag band, so it ends exactly where the first
    // player row begins.
    let panel = ui.available_rect_before_wrap();
    // The header band's full paint extent — `panel` truncated to
    // `band_height`, with no top adjustment. `RESIZE_EDGE` is a
    // hit-test-only concern (see `band` below), not a paint boundary; the
    // panel's own fill/border already cover that strip.
    let header_paint_clip =
        egui::Rect::from_min_size(panel.min, egui::vec2(panel.width(), band_height));
    // The height the gutter emblem's box is *centered* on — the header's
    // text rows, which is the block the mark decorates. Not what it is
    // clipped to: that is `header_paint_clip`, the whole band (see the blit
    // below).
    let text_band_height = header_text_band_height();
    // The whole header band is the drag surface — title line, subtitle
    // line, and the timer/DPS/buttons row — registered *before* the
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
    // subtitle, stat row — sits on top of it. Sized to the whole drag band
    // (issue #91), not the text rows alone (issue #81) and not the fixed
    // 98pt run before that: the gradient and the oversized emblem it carries
    // are the header's background, so they run behind the stat-pill row too.
    //
    // Issue #158: `band_height` alone stops 8pt short of the first player
    // row — `OverlayApp::ui` puts a `ui.separator()` between the header and
    // the rows, and pays the layout's ordinary `ITEM_SPACING_Y` gap before
    // it, neither of which is inside the band. Sizing the wash to just
    // `band_height` left that 8pt strip showing the bare panel fill (with
    // the separator's faint line inside it) between the wash's bottom edge
    // and the first row — a hard, visible cutoff, not a fade. Extending to
    // `first_player_row_top_offset` — the same function `default_inner_
    // height` sums for the window's default open height — closes that gap
    // and keeps the two derivations from ever drifting apart again. Never a
    // literal.
    let wash_height = first_player_row_top_offset(band_height) - HEADER_WASH_INSET;
    draw_header_wash(ui, panel, icons, wash_height);

    // Issue #183: pinning the overlay locks its *position* as well as its
    // Z-order, so the drag band refuses to start a move while
    // `Settings::always_on_top` is set, and says so with its cursor.
    let drag_locked = drag_locked_by_pin(settings.settings.always_on_top);
    let drag_surface = ui.interact(band, ui.id().with("title_bar"), egui::Sense::drag());
    if drag_surface.hovered() {
        ctx.set_cursor_icon(if drag_locked {
            egui::CursorIcon::NotAllowed
        } else {
            egui::CursorIcon::Grab
        });
    }
    // Once per gesture: this only captures the anchor the drag is measured
    // against. The actual per-frame repositioning is `drive_window_gesture`.
    if !drag_locked && drag_surface.drag_started_by(egui::PointerButton::Primary) {
        begin_window_gesture(ctx, gesture, GestureKind::Move);
    }

    // Title is always rendered (even as the "No target" placeholder) so the
    // header's height never jitters between frames. So is the subtitle row
    // (issue #91): it renders empty when the scene is unknown rather than
    // being skipped, because skipping it collapsed the band by
    // `SUBTITLE_LINE_HEIGHT + ITEM_SPACING_Y` and lifted the whole stat-pill
    // row every time the app fell back to its idle "No target" state.
    let title_row = draw_title_line(ui, &title);
    // The gutter emblem (issue #59): bled off the left edge and centered on
    // the header's *text* rows (`text_band_height`), but clipped to the
    // *whole band* (`header_paint_clip`, issue #91). The two are deliberately
    // different heights and neither is a typo for the other.
    //
    // Issue #75 clipped this to the text band as well, to keep the mark's
    // lower blade off the timer's readout underneath. That cost the diamond
    // its bottom corner: the box runs 14pt past the text band, so a text-band
    // clip visibly chopped the mark's point off mid-blade. The stat row is
    // what moved instead (`HEADER_STAT_ROW_INSET_X`) — it now starts right
    // of the emblem's right edge, so the two share the same rows without
    // touching, and the clip is free to be the band's. It is a floor, not a
    // trim: all it still stops is ink running on into the first player row.
    //
    // The mark's *top* corner is still cut, by the panel's own edge — see
    // `header_emblem_rect`, where that asymmetry is the design.
    if let Some(emblem) = icons.glyphs.get(GlyphIcon::Emblem) {
        ui.painter().with_clip_rect(header_paint_clip).image(
            emblem.id(),
            header_emblem_rect(title_row, text_band_height),
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
    // The title row's own toggle pill (issue #185): click-through and
    // always-on-top, immediately left of the chevron. Registered after the
    // drag surface (so its clicks never start a window move) and before the
    // chevron, which it does not overlap.
    title_row_toggles(
        ui,
        SettingsHandle {
            settings: &mut *settings.settings,
            tx_settings: settings.tx_settings,
        },
        icons,
        title_row,
        capturing,
    );
    // Issue #183: a pin click above can land *mid-drag* — the pointer is
    // necessarily still down on the header band that started the move — so
    // the in-flight gesture is cancelled here, on the same frame, before
    // `OverlayApp::ui`'s `drive_window_gesture` gets to advance it. Gating
    // the gesture's *start* alone would have let that one drag run to
    // completion after the window was supposedly locked.
    cancel_move_gesture_when_pinned(gesture, settings.settings.always_on_top);
    let chevron_response = menu_chevron(ui, chevron_rect(title_row));
    // `CloseOnClickOutside` rather than the default `CloseOnClick` (issue
    // #93): with the Columns checkboxes now direct children of this popup
    // (no submenu layer to defer the close decision to, see
    // `draw_header_menu`'s doc comment), the default would dismiss the
    // whole dropdown on every checkbox toggle. Minimize/Close call
    // `ui.close()` themselves to still dismiss on click.
    // `settings` (a `SettingsHandle<'_>`) is reborrowed rather than moved
    // into this closure — issue #167 added a second use of it below
    // (`toggle_cluster`), and `SettingsHandle`'s `&mut Settings` field
    // can't be moved out twice. `&mut *settings.settings` is an ordinary
    // reborrow through that field, scoped to just this `.show(...)` call,
    // so `settings` is still whole and usable afterward.
    egui::Popup::menu(&chevron_response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            draw_header_menu(
                ui,
                ctx,
                tx_command,
                SettingsHandle {
                    settings: &mut *settings.settings,
                    tx_settings: settings.tx_settings,
                },
                icons,
                update_check,
                tx_log_export,
            );
        });
    draw_subtitle_line(ui, subtitle.as_deref().unwrap_or(""));

    // The gap above the stat row is `HEADER_STAT_ROW_GAP`, not the layout's
    // own `ITEM_SPACING_Y` — see that constant for why, and
    // `header_band_height` for the budget it feeds. egui already inserted
    // one `ITEM_SPACING_Y` after the row above, so only the remainder is
    // added here.
    ui.add_space(HEADER_STAT_ROW_GAP - ITEM_SPACING_Y);
    ui.horizontal(|ui| {
        // The whole row is inset from the panel's left content edge
        // (`HEADER_STAT_ROW_INSET_X`, issue #91). `add_space` in a
        // horizontal layout advances the cursor directly, with no
        // `item_spacing` of its own, so the timer's ink starts exactly this
        // far in.
        ui.add_space(HEADER_STAT_ROW_INSET_X);
        // Three stat readouts (issue #56), replacing the bare
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
        // Total damage for the fight (reference render's e.g. "30.10B"). The
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
        toggle_cluster(
            ui,
            tx_command,
            icons,
            capturing,
            share_active,
            has_history,
            open_history,
        )
    })
    .inner
}

// -- toggle cluster (issue #62, #82, #167) --------------------------------
//
// The source's fourth stat-row cell: a click-through LED, a cloud-upload LED
// and a queue gauge, in a 22pt pill. We had none of those features, so all
// three used to render **in their off state and inert** — no click
// handling, no settings, no tooltip. Issue #62 was explicit that a use for
// these slots would be decided later; issue #82 repurposed the click-through
// and cloud-upload LEDs as real buttons (Share — copy a screenshot to the
// clipboard — and Reset, moved out of the header dropdown), leaving only the
// queue gauge ring inert, since there was still no upload queue for it to
// show. Issue #167 dropped that still-inert ring entirely (there is still no
// queue, and none planned) and added two real, unrelated toggles in its
// place and one new slot alongside it: OS-level mouse click-through and
// runtime always-on-top (`ViewportCommand::WindowLevel`).
//
// Issue #185 then split that four-button pill in two: click-through and
// always-on-top moved up to the title row, into their own oval left of the
// dropdown chevron (`title_row_toggles`), and issue #186 filled one of the
// slots they freed with History, moved out of the header dropdown. So this
// pill now holds three one-shot actions — Share, Reset, History — the title
// row's holds the two on/off toggles, and nothing anywhere in either is
// decorative.
//
// Click-through (issue #167 rehash) is *not* `ViewportCommand::
// MousePassthrough` — that sets `WS_EX_TRANSPARENT` on the whole window
// with no per-region carve-out, so once it was on, the very button that
// turns it back off became unclickable too, with only a keyboard hotkey
// (which needs focus that click-through's whole point is to give away) as
// an escape hatch. Instead, `platform::set_click_through` flips a flag
// `platform::window_proc`'s `WM_NCHITTEST` handling reads directly: it
// reports `HTTRANSPARENT` for the whole window *except* the click-through
// button's own hit box (published every frame below via `platform::
// set_click_through_button_rect`), which always reports normal
// (`HTCLIENT`) hit-testing. The button is therefore reachable with the
// mouse in every state — it is deliberately excluded from its own
// passthrough — and the tray menu's "Turn off click-through" entry
// (`platform::install_tray`) exists only as a belt-and-braces fallback for
// the case that carve-out ever fails or the window ends up off-screen.

/// Tint a toggle-cluster button paints with while its state is "off" — the
/// source's `OffBrush="#1fff"`, originally the still-inert queue ring's
/// stroke color, now `toggle_state_tint`'s off case for the click-through
/// and always-on-top buttons (issue #167).
const TOGGLE_OFF_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x11);
/// Tint the toggle cluster's buttons are painted with while active — the
/// same half-white `TOOLBAR_ICON_TINT` every other clickable icon in this
/// module uses. Share and Reset (one-shot actions, not on/off state) always
/// paint at this tint; click-through and always-on-top use it only in their
/// "on" state (`toggle_state_tint`).
const TOGGLE_ACTIVE_COLOR: egui::Color32 = TOOLBAR_ICON_TINT;
/// Circular hover wash painted behind a toggle-cluster button, matching the
/// oval pill's own shape rather than a foreign square badge.
const TOGGLE_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 30);
const TOGGLE_MOUSE_SIDE: f32 = 12.0;
const TOGGLE_CLOUD_SIDE: f32 = 14.0;
/// Click-through button glyph side (issue #167) — same 14pt as `TOGGLE_
/// CLOUD_SIDE`/the old queue slot, keeping every non-Share button in the
/// cluster the same visual size.
const TOGGLE_CLICK_THROUGH_SIDE: f32 = 14.0;
/// Always-on-top button glyph side (issue #167) — see `TOGGLE_CLICK_
/// THROUGH_SIDE`'s doc comment.
const TOGGLE_ALWAYS_ON_TOP_SIDE: f32 = 14.0;
/// History button glyph side (issue #186) — the same 14pt as every other
/// non-Share button in the cluster, see `TOGGLE_CLICK_THROUGH_SIDE`.
const TOGGLE_HISTORY_SIDE: f32 = 14.0;
const TOGGLE_GAP: f32 = 5.0;
const TOGGLE_PAD_X: f32 = 4.0;

/// Gap, in points, between the title row's toggle pill (issue #185) and the
/// dropdown chevron's reserved strip to its right. `TOGGLE_PAD_X`'s value,
/// deliberately: the pill's own internal padding is what sets the rhythm the
/// chevron then continues, so the two read as one run of controls rather
/// than two clusters that happen to be adjacent.
const TITLE_TOGGLE_GAP_X: f32 = TOGGLE_PAD_X;

/// Width of the title row's toggle pill (issue #185): the click-through and
/// always-on-top buttons, laid out with exactly the padding, glyph sides and
/// inter-button gap they had inside `toggle_cluster`, so the two ovals still
/// read as one family after the move.
const TITLE_TOGGLE_PILL_WIDTH: f32 =
    2.0 * TOGGLE_PAD_X + TOGGLE_CLICK_THROUGH_SIDE + TOGGLE_GAP + TOGGLE_ALWAYS_ON_TOP_SIDE;

/// Width the *title* row keeps clear at its right end (issue #185): the
/// chevron's own `HEADER_RIGHT_CONTROL_WIDTH` strip, plus the toggle pill
/// that now sits immediately left of it and the gap between them. Its own
/// constant rather than a wider `HEADER_RIGHT_CONTROL_WIDTH` because the
/// pill lives on the title row alone — the subtitle row still reserves only
/// the chevron strip, and widening the shared constant would have punched a
/// 45pt hole in the area name for no reason.
const TITLE_RIGHT_CONTROLS_WIDTH: f32 =
    HEADER_RIGHT_CONTROL_WIDTH + TITLE_TOGGLE_GAP_X + TITLE_TOGGLE_PILL_WIDTH;
/// Points the click-through button's published hit box (`platform::
/// set_click_through_button_rect`) is padded out by on every side, so the
/// `WM_NCHITTEST` carve-out that keeps it reachable under click-through
/// (issue #167 rehash) isn't razor-thin right at the glyph's edges.
const CLICK_THROUGH_HIT_PAD: f32 = 2.0;

/// Horizontal offset, in points, from the title row's toggle pill's left
/// edge to the left edge of the click-through button's glyph box. Issue #185
/// moved this button out of the stat row's toggle cluster and into the title
/// pill, where click-through is the *first* slot, so the offset is now the
/// pill's own left padding and nothing else — still spelled as its own
/// constant so the hit box can be computed before the pill is painted (see
/// `click_through_button_slot`).
const CLICK_THROUGH_SLOT_OFFSET_X: f32 = TOGGLE_PAD_X;

/// The click-through button's glyph box, in points, derived from the title
/// row's toggle pill rect (`title_toggle_pill_rect`). Pure so
/// `title_row_toggles` can publish the button's `WM_NCHITTEST` hit box
/// (issue #167 rehash) before it starts painting — and so the geometry is
/// unit-testable on every platform, unlike the `cfg(windows)` publish
/// itself.
fn click_through_button_slot(pill: egui::Rect) -> egui::Rect {
    egui::Rect::from_center_size(
        egui::pos2(
            pill.left() + CLICK_THROUGH_SLOT_OFFSET_X + TOGGLE_CLICK_THROUGH_SIDE / 2.0,
            pill.center().y,
        ),
        egui::Vec2::splat(TOGGLE_CLICK_THROUGH_SIDE),
    )
}

/// Converts the click-through button's egui rect (points, already
/// client-area-relative — matching what `ScreenToClient` produces) into the
/// physical-pixel `left, top, right, bottom` bounds `platform::
/// set_click_through_button_rect` publishes for `WM_NCHITTEST`. Padded by
/// `CLICK_THROUGH_HIT_PAD` points first so the carve-out isn't razor-thin at
/// the glyph's edges.
///
/// A non-finite or non-positive `pixels_per_point` is treated as `1.0`, the
/// same guard (and for the same reason) as `platform::cursor_points`:
/// multiplying by `0.0`/NaN would round every bound to `0`, and a
/// zero-area rect can never `contains` anything — so `click_through_hit_test`
/// would resolve `Transparent` for *every* point, including the button that
/// turns click-through back off. Identity scaling leaves the button a few
/// pixels off; a degenerate rect strands the user.
///
/// Pure so the points-to-pixels transform is unit-testable off-Windows; the
/// `cfg`-gated publish call is the only Windows-only part.
fn click_through_hit_box_px(
    button_rect: egui::Rect,
    pixels_per_point: f32,
) -> (i32, i32, i32, i32) {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let padded = button_rect.expand(CLICK_THROUGH_HIT_PAD);
    (
        (padded.left() * scale).round() as i32,
        (padded.top() * scale).round() as i32,
        (padded.right() * scale).round() as i32,
        (padded.bottom() * scale).round() as i32,
    )
}

/// One toggle-cluster button's hit box, hover highlight and accessible
/// label. The painted glyph itself is the caller's job — Share and Reset
/// draw from two different icon sets (`GlyphIcon` and `ToolbarIcon`) — so
/// this only owns the interaction chrome shared by both. Same
/// hand-supplied `widget_info`/`on_hover_text` shape as `menu_chevron`, and
/// for the same reason: a raw `interact` `Response` carries no `WidgetInfo`
/// from anywhere.
///
/// `capturing` (issue #156) suppresses both the hover circle and the
/// tooltip while a Share screenshot capture is in flight: the pointer is
/// necessarily still over whichever button was clicked to start the
/// capture, so on the frame that actually gets captured (see
/// `screenshot_capture_guard`'s doc comment — it's *not* the click frame
/// itself) both would otherwise paint straight into the image. Applies to
/// every button in the cluster, not just Share, since `capturing` is one
/// flag shared by the whole row.
///
/// `enabled` (PR #197 review) is the widget's real interactive state, not a
/// paint hint: `false` drops it to `Sense::hover()`, so it registers no
/// click and accesskit publishes no `Action::Click`, and reports
/// `enabled: false` in the `WidgetInfo`, which is what makes a screen
/// reader announce it as disabled. That is the `add_enabled(..)` behaviour
/// the History item had back in `draw_header_menu`; gating only the click's
/// *effect* at the call site left the button sounding perfectly usable
/// while doing nothing.
fn toggle_button(
    ui: &egui::Ui,
    rect: egui::Rect,
    label: &str,
    capturing: bool,
    enabled: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("toggle_cluster", label)),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if response.hovered() && !capturing {
        ui.painter()
            .circle_filled(rect.center(), rect.width() / 2.0 + 2.0, TOGGLE_HOVER_FILL);
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled && response.enabled(),
            label,
        )
    });
    if capturing {
        response
    } else {
        response.on_hover_text(label)
    }
}

/// Which tint a toggle-cluster on/off button paints its glyph with (issue
/// #167): `TOGGLE_ACTIVE_COLOR` — the same half-white every clickable icon
/// in this cluster uses — while the state is on, `TOGGLE_OFF_COLOR` — the
/// near-invisible tint the old inert queue ring painted with — while off.
/// Pure so the state -> color mapping is unit-testable without a live
/// `egui::Context`; `toggle_cluster` is the only caller.
fn toggle_state_tint(active: bool) -> egui::Color32 {
    if active {
        TOGGLE_ACTIVE_COLOR
    } else {
        TOGGLE_OFF_COLOR
    }
}

/// The click-through tray escape hatch (issue #167 rehash). The button
/// itself is always clickable now (`platform::window_proc`'s `WM_NCHITTEST`
/// carve-out — see the section comment above), so this is only the
/// belt-and-braces fallback for when the tray's "Turn off click-through"
/// entry fires: `requested` is whatever `OverlayApp::ui` read from
/// `platform::take_tray_click_through_off_request` this frame.
///
/// Unlike the deleted `Ctrl+Alt+P` hotkey this replaced, a tray request
/// only ever turns click-through *off* — there's no tray item for turning
/// it on, since the always-reachable button already owns that — so this is
/// not a toggle: a request forces `false` outright, and an idle frame
/// leaves `click_through` exactly as it found it. Pure so this is
/// unit-testable without a live `egui::Context` or the platform layer's
/// real atomics.
fn click_through_after_tray_request(click_through: bool, requested: bool) -> bool {
    if requested { false } else { click_through }
}

/// Paints the stat row's toggle cluster: Share, Reset (issue #82) and
/// History (issue #186) — three one-shot actions in one `PILL_FILL` oval,
/// matching the DPS/damage pills' chrome. Click-through and always-on-top
/// used to live here too; issue #185 moved them to the title row's own pill
/// (`title_row_toggles`), and History took one of the slots they freed.
///
/// Returns whether Share was clicked this frame (issue #96, PR #98 review)
/// — `draw_header` propagates this up so `OverlayApp::ui` knows to stash
/// this frame's row-bottom bound for the async screenshot reply.
///
/// `capturing` (issue #156) is threaded straight through to every
/// `toggle_button` call so the whole cluster suppresses its hover fill and
/// tooltip together while a capture is in flight, rather than special-casing
/// Share alone.
///
/// `has_history`/`open_history` (issue #186) are the History item's old
/// `draw_header_menu` parameters, rerouted here with the button: the cluster
/// no longer needs a `SettingsHandle` at all, since the only two controls
/// that flipped a setting are the ones that left.
///
/// `share_active` (issue #219) is whether Share may fire at all this frame
/// — see `share_active_for_view`'s doc comment. Gated the same way
/// `has_history` gates History: `toggle_button`'s `enabled` flag (so a
/// disabled Share senses no click and accesskit reports it disabled) plus
/// `toggle_state_tint` for the dimmed glyph, rather than hiding the button
/// outright — hiding it would shift every button to its right on the
/// frames it's inactive.
/// The "<label>" / "<label>: unavailable" shape shared by every toggle
/// whose label doubles as its own disabled-state explanation (Share's
/// `share_active`, History's `has_history`, PR #225 review of issue #219)
/// — pulled out so the wording can't drift apart between the two call
/// sites by a hand-edit to just one of them.
fn availability_label(
    active: bool,
    label: &'static str,
    unavailable: &'static str,
) -> &'static str {
    if active { label } else { unavailable }
}

fn toggle_cluster(
    ui: &mut egui::Ui,
    tx_command: &Sender<UiCommand>,
    icons: &Icons,
    capturing: bool,
    share_active: bool,
    has_history: bool,
    open_history: &mut bool,
) -> bool {
    let height = ui.spacing().interact_size.y;
    let width = 2.0 * TOGGLE_PAD_X
        + TOGGLE_MOUSE_SIDE
        + TOGGLE_GAP
        + TOGGLE_CLOUD_SIDE
        + TOGGLE_GAP
        + TOGGLE_HISTORY_SIDE;
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    if !ui.is_rect_visible(rect) {
        return false;
    }
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, PILL_FILL);

    let mut x = rect.left() + TOGGLE_PAD_X;
    let y = rect.center().y;

    // Share (issue #82): captures the app window as rendered this instant
    // and copies it to the system clipboard as an image, replacing a manual
    // screenshot. The capture is asynchronous — this only fires the
    // request; `handle_screenshot_events` picks up the resulting
    // `Event::Screenshot` on a later frame and does the actual clipboard
    // write.
    let share_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_MOUSE_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_MOUSE_SIDE),
    );
    let mut screenshot_requested = false;
    let share_label = availability_label(
        share_active,
        "Copy screenshot to clipboard",
        "Copy screenshot to clipboard: unavailable",
    );
    if toggle_button(ui, share_rect, share_label, capturing, share_active).clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        screenshot_requested = true;
    }
    if let Some(share) = icons.glyphs.get(GlyphIcon::Share) {
        ui.painter().image(
            share.id(),
            share_rect,
            UV_FULL,
            toggle_state_tint(share_active),
        );
    }
    x += TOGGLE_MOUSE_SIDE + TOGGLE_GAP;

    // Reset (issue #82): moved out of the header dropdown — `draw_header_menu`
    // used to be its only trigger — into a one-click slot here, reusing the
    // same `ToolbarIcon::Reset` texture and the same `UiCommand::Reset`.
    let reset_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_CLOUD_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_CLOUD_SIDE),
    );
    if toggle_button(ui, reset_rect, "Reset", capturing, true).clicked() {
        let _ = tx_command.try_send(UiCommand::Reset);
    }
    if let Some(reset) = icons.toolbar.get(ToolbarIcon::Reset) {
        ui.painter()
            .image(reset.id(), reset_rect, UV_FULL, TOGGLE_ACTIVE_COLOR);
    }
    x += TOGGLE_CLOUD_SIDE + TOGGLE_GAP;

    // History (issue #186): moved out of the header dropdown into the slot
    // issue #185 freed by taking click-through to the title row. Same
    // `open_history` flag the menu item used to set, read back by
    // `OverlayApp::ui` to switch `self.view` into `OverlayView::History`.
    //
    // The disabled state the menu item carried (`add_enabled(has_history,
    // ..)`, for history off in settings.json or a database that could not be
    // opened) survives the move as `toggle_state_tint(has_history)` — the
    // same dim "off" tint the click-through and always-on-top buttons wear,
    // which is what "inert" already looks like in this chrome — plus
    // `toggle_button`'s `enabled` flag, which carries the half of
    // `add_enabled` the tint cannot (PR #197 review): gating only the
    // click's effect here left the widget sensing clicks and telling
    // accesskit it was enabled, so a screen reader announced a usable
    // button that did nothing. The tooltip is what explains it, so the
    // button is still self-describing rather than silently inert.
    let history_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_HISTORY_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_HISTORY_SIDE),
    );
    let history_label = availability_label(has_history, "History", "History: unavailable");
    if toggle_button(ui, history_rect, history_label, capturing, has_history).clicked() {
        *open_history = true;
    }
    if let Some(history) = icons.glyphs.get(GlyphIcon::History) {
        ui.painter().image(
            history.id(),
            history_rect,
            UV_FULL,
            toggle_state_tint(has_history),
        );
    }

    screenshot_requested
}

/// Paints the title row's toggle pill (issue #185): click-through and
/// always-on-top, moved out of `toggle_cluster` into their own stadium
/// immediately left of the dropdown chevron, wearing the same `PILL_FILL`
/// chrome and the same `toggle_button`/`toggle_state_tint` treatment they
/// had in the stat row. Only the position changed.
///
/// Registered with `ui.interact` on explicit rects rather than allocated,
/// exactly like `menu_chevron`: this lives *inside* the title row
/// `draw_title_line` already allocated, in the strip `title_text_rect` keeps
/// clear for it. It is registered after `draw_header`'s title-bar drag
/// surface, so it wins the hit test over it and clicking a toggle never
/// starts a window drag.
///
/// It is also the one publisher of the click-through button's
/// `WM_NCHITTEST` hit box, which followed the button here — see the
/// invariant below.
fn title_row_toggles(
    ui: &mut egui::Ui,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    title_row: egui::Rect,
    capturing: bool,
) {
    let SettingsHandle {
        settings,
        tx_settings,
    } = settings;

    let rect = title_toggle_pill_rect(title_row, ui.spacing().interact_size.y);

    // Invariant: the published hit box always describes *this* frame's
    // layout, even on a frame egui culls the pill — publishing before the
    // visibility early return below is what guarantees it, because a culled
    // frame that returned early would leave `WM_NCHITTEST` consulting a rect
    // from a previous layout and a click at the button's real position would
    // resolve `Transparent`, stranding the user under click-through. Clearing
    // the rect on the culled path was the alternative; publishing wins
    // because it keeps the button reachable rather than merely un-stale.
    // See `click_through_hit_box_px` for the points-to-pixels conversion and
    // `platform::CLICK_THROUGH_BUTTON_LEFT` for why it's unconditional.
    let click_through_rect = click_through_button_slot(rect);
    let (hit_left, hit_top, hit_right, hit_bottom) =
        click_through_hit_box_px(click_through_rect, ui.ctx().pixels_per_point());
    crate::platform::set_click_through_button_rect(hit_left, hit_top, hit_right, hit_bottom);

    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, PILL_FILL);

    // Click-through (issue #167; mechanism replaced — issue #167 rehash,
    // then completed by issue #183): OS-level mouse passthrough for the
    // whole overlay *except this very button*, which stays clickable in
    // every state by design — see the toggle-cluster section comment above
    // for the carve-out, and `platform::click_through_passthrough_wanted`
    // for why `WM_NCHITTEST` alone could never pass a click to another
    // process and what now does. `GlyphIcon::MouseOff`'s literal "mouse
    // disabled" glyph is the closest fit the vendored set has. Persisted to
    // `Settings::click_through` so it survives a restart, re-applied on
    // `OverlayApp`'s first frame. The tray menu's "Turn off click-through"
    // entry is a belt-and-braces fallback, not the primary way back — this
    // button is.
    let click_through_label = if settings.click_through {
        "Click-through: on"
    } else {
        "Click-through: off"
    };
    if toggle_button(ui, click_through_rect, click_through_label, capturing, true).clicked() {
        settings.toggle_click_through();
        crate::platform::set_click_through(settings.click_through);
        let _ = tx_settings.send(settings.clone());
    }
    if let Some(mouse_off) = icons.glyphs.get(GlyphIcon::MouseOff) {
        ui.painter().image(
            mouse_off.id(),
            click_through_rect,
            UV_FULL,
            toggle_state_tint(settings.click_through),
        );
    }

    // Always-on-top (issue #167): whether the overlay stays pinned above
    // other windows, via `ViewportCommand::WindowLevel` — the runtime
    // counterpart to `viewport()`'s hardcoded `.with_always_on_top()`,
    // which only ever sets the *initial* level a fresh process opens with.
    // Issue #183: it now also locks the window's *position* — see
    // `drag_locked_by_pin` — so "pinned" means pinned in both senses rather
    // than a Z-order change the user can still shove around by accident.
    // `GlyphIcon::Pin` — MDI's pushpin glyph — is a literal fit for
    // "pinned above other windows". Persisted to `Settings::always_on_top`
    // the same way `click_through` is.
    let always_on_top_rect = egui::Rect::from_center_size(
        egui::pos2(
            click_through_rect.right() + TOGGLE_GAP + TOGGLE_ALWAYS_ON_TOP_SIDE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(TOGGLE_ALWAYS_ON_TOP_SIDE),
    );
    let always_on_top_label = if settings.always_on_top {
        "Always on top: on"
    } else {
        "Always on top: off"
    };
    if toggle_button(ui, always_on_top_rect, always_on_top_label, capturing, true).clicked() {
        settings.toggle_always_on_top();
        let level = if settings.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        let _ = tx_settings.send(settings.clone());
    }
    if let Some(pin) = icons.glyphs.get(GlyphIcon::Pin) {
        ui.painter().image(
            pin.id(),
            always_on_top_rect,
            UV_FULL,
            toggle_state_tint(settings.always_on_top),
        );
    }
}

/// Scans this frame's input events for the reply to a screenshot request
/// fired by the Share button (`toggle_cluster`'s
/// `ViewportCommand::Screenshot`) and hands the captured image to `write`.
/// Split out from the actual clipboard call so the routing — "a
/// `Screenshot` event reaches the writer" — is unit-testable with a fake
/// `write` and no live window or clipboard; the real call site
/// (`OverlayApp::ui`) passes `platform::write_clipboard_image`.
///
/// `write` takes the event's `Arc<ColorImage>` itself rather than a bare
/// `&ColorImage`: `egui::Event::Screenshot`'s `image` field is already an
/// `Arc`, and handing out the reference-counted handle (a cheap refcount
/// bump) instead of a borrow lets callers that need to keep the image past
/// this closure — `OverlayApp::ui` collects every landed image into a
/// `Vec` for the crop/write loop below — do so without a full pixel-buffer
/// deep copy (`epaint::ColorImage` derives `Clone` over `Vec<Color32>`,
/// which for a 4K capture is tens of MB).
///
/// A `Screenshot` event can only be `Event::Screenshot` in practice, but
/// nothing stops another viewport's reply from showing up in a multi-
/// viewport app; this app only ever has the root viewport, so every event
/// this frame is implicitly ours.
fn handle_screenshot_events(
    ctx: &egui::Context,
    mut write: impl FnMut(std::sync::Arc<egui::ColorImage>),
) {
    ctx.input(|input| {
        for event in &input.events {
            if let egui::Event::Screenshot { image, .. } = event {
                write(image.clone());
            }
        }
    });
}

/// Issue #96: the window-space y coordinate (in points) of the bottom of
/// the row list's actual content — top chrome plus only the populated rows
/// — used to crop the Share button's clipboard screenshot down to just
/// that content instead of the whole window.
///
/// `rows_area_height` is the row `ScrollArea`'s own allocated height: per
/// `draw_rows`'s `auto_shrink([false, false])` it always fills whatever
/// space the panel leaves it, so it is exactly what got rendered this
/// frame, regardless of how many rows there are. Clamping the rows' own
/// height to it handles both crop directions purely:
///
/// - Trailing empty space (fewer/shorter rows than the scroll area) is
///   trimmed — the bound stops right after the last row, not at the
///   scroll area's full height.
/// - Scrolled-off rows (more row content than the scroll area shows) are
///   never trimmed further than what was actually painted — the bound
///   clamps at the scroll area's height rather than the taller row-count
///   total, since content past that was clipped out of the render itself
///   and cannot be recovered from a screenshot of it.
fn rows_content_bottom_y(
    rows_top: f32,
    row_count: usize,
    row_height: f32,
    rows_area_height: f32,
) -> f32 {
    let rows_height = row_count as f32 * row_height;
    rows_top + rows_height.min(rows_area_height.max(0.0))
}

/// Converts a window-space y bound (points, from `rows_content_bottom_y`)
/// to a row count in the captured `ColorImage`'s physical-pixel coordinate
/// space, using the frame's `pixels_per_point` scale. Clamped to
/// `image_height_px` as a last-resort safety net — `pixels_per_point` is
/// still read fresh on the frame the reply lands on (see
/// `OverlayApp::pending_screenshot_bound`'s doc comment, which covers the
/// row-count half of this), so a DPI/zoom change mid-round-trip could in
/// principle still scale the bound past the image; this clamp makes sure
/// that only ever crops *less* than the full image, never indexes past its
/// end.
fn screenshot_crop_height_px(
    bottom_y_points: f32,
    pixels_per_point: f32,
    image_height_px: usize,
) -> usize {
    // Clippy's `neg_cmp_op_on_partial_ord`: `!(x > 0.0)` is not the same as
    // `x <= 0.0` once NaN is possible (NaN compares false either way, so
    // the negated form silently treats it as "not positive" too — which is
    // exactly the fallback wanted here, but spelled via `partial_cmp` to
    // make that NaN handling explicit rather than implicit in a negation).
    let is_positive = |v: f32| v.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater);
    if !is_positive(bottom_y_points) || !is_positive(pixels_per_point) {
        return 0;
    }
    let px = bottom_y_points * pixels_per_point;
    if !px.is_finite() {
        return image_height_px;
    }
    (px.round() as usize).min(image_height_px)
}

/// Crops a Share-button screenshot down to the header chrome plus
/// populated rows (issue #96), dropping trailing empty window background
/// below the last row — or keeping the full capture when the content
/// already fills it (scrolled-off rows, or the bound clamped past the
/// image). This is a pure post-capture crop of the `ColorImage` already in
/// hand: it never touches the live window's size or geometry, which is the
/// point of doing it here rather than by resizing the overlay — only the
/// clipboard image differs from a bare full-window capture.
///
/// Issue #82: a `crop_height` of `0` — from a non-positive bound or
/// `pixels_per_point`, per `screenshot_crop_height_px`'s own fallback —
/// also keeps the full image rather than producing a genuinely
/// zero-height `ColorImage`. arboard rejects a zero-height image outright
/// (`Error::ConversionFailure`), which is what made the Share button fail
/// on every single click; a full, uncropped screenshot is a far better
/// failure mode than no screenshot at all.
///
/// Takes and returns `Arc<ColorImage>`, not a bare `&ColorImage`/
/// `ColorImage`, so the `crop_height == 0` no-op path — which is exactly
/// the issue-#82 case above, not a rare edge — returns via a cheap Arc
/// refcount bump instead of a second full pixel-buffer deep copy on top of
/// the one `handle_screenshot_events`'s caller already avoids.
fn crop_screenshot_to_rows(
    image: &std::sync::Arc<egui::ColorImage>,
    bottom_y_points: f32,
    pixels_per_point: f32,
) -> std::sync::Arc<egui::ColorImage> {
    let [width, height] = image.size;
    let crop_height = screenshot_crop_height_px(bottom_y_points, pixels_per_point, height);
    if crop_height == 0 || crop_height >= height {
        return image.clone();
    }
    let pixels = image.pixels[..width * crop_height].to_vec();
    std::sync::Arc::new(egui::ColorImage::new([width, crop_height], pixels))
}

/// Issue #183: flattens a captured screenshot's alpha before the clipboard
/// write, so what lands on the clipboard is a visible image rather than a
/// mostly-invisible one.
///
/// The overlay is a transparent, borderless window: `PANEL_FILL` is painted
/// at `Settings::opacity` and the rounded corners are fully transparent, so
/// a `ViewportCommand::Screenshot` capture legitimately comes back with
/// translucent pixels. `epaint::Color32` stores those channels
/// *premultiplied*, while `arboard::ImageData` is straight (non-
/// premultiplied) RGBA, and on Windows arboard passes the alpha straight
/// through into the `CF_DIBV5` it writes. Pasted anywhere that honours that
/// alpha, the result is a near-invisible image — which from the outside is
/// indistinguishable from "the Copy button does nothing", with no error
/// anywhere for the log to catch.
///
/// Forcing every pixel opaque *is* compositing the capture over black,
/// precisely because the channels are already premultiplied: the RGB bytes
/// are correct as they stand and only the alpha byte changes. That is what
/// the overlay looks like over a dark game frame, and it pastes in every
/// clipboard consumer instead of none of them.
///
/// Returns the input `Arc` untouched when the capture is already fully
/// opaque — a refcount bump rather than a second full pixel-buffer copy,
/// same reasoning as `crop_screenshot_to_rows`'s no-op path.
fn flatten_screenshot_alpha(
    image: &std::sync::Arc<egui::ColorImage>,
) -> std::sync::Arc<egui::ColorImage> {
    if image.pixels.iter().all(|pixel| pixel.a() == u8::MAX) {
        return image.clone();
    }
    let pixels = image
        .pixels
        .iter()
        .map(|pixel| {
            egui::Color32::from_rgba_premultiplied(pixel.r(), pixel.g(), pixel.b(), u8::MAX)
        })
        .collect();
    std::sync::Arc::new(egui::ColorImage::new(image.size, pixels))
}

/// Decides what row bound a landed screenshot reply should crop to, and
/// what `OverlayApp::pending_screenshot_bound` must hold afterward.
///
/// Issue #96 (PR #98 review) `take()`s the pending bound so a stale value
/// can never be reused by a later, unrelated frame — but the original fix
/// took it *unconditionally*, once per frame, regardless of whether that
/// frame actually had a screenshot event to handle. `ViewportCommand::
/// Screenshot`'s reply is asynchronous and can land any number of frames
/// after the Share click that requested it, so the bound stashed on the
/// request frame was thrown away on the very next idle frame — long
/// before the reply could arrive. By reply time `pending_screenshot_bound`
/// was already `None`, `unwrap_or(0.0)` handed `crop_screenshot_to_rows` a
/// zero bound, and the resulting zero-height image made arboard reject
/// the clipboard write with `Error::ConversionFailure` on every attempt
/// (issue #82).
///
/// The fix: only consume (reset to `None`) the pending bound on the frame
/// that actually handles a screenshot event (`event_landed`); every other
/// frame must leave whatever is pending untouched so it survives however
/// many frames the round trip takes.
fn take_pending_screenshot_bound(pending: Option<f32>, event_landed: bool) -> (f32, Option<f32>) {
    if event_landed {
        (pending.unwrap_or(0.0), None)
    } else {
        (0.0, pending)
    }
}

/// Issue #156: the state transition behind `OverlayApp::screenshot_
/// capturing`, the guard `toggle_cluster`'s buttons read to decide whether
/// to suppress their hover fill and tooltip this frame.
///
/// `egui::ViewportCommand::Screenshot`'s own doc comment says it captures
/// "the next frame after" the one that sends it — not the request frame
/// itself — and its `Event::Screenshot` reply is asynchronous, landing any
/// number of frames later (the same round trip `take_pending_screenshot_
/// bound` accounts for). So the guard can't just be "true on the frame
/// `toggle_cluster` fires the request": it has to still be `true` on the
/// *next* frame (the one actually captured), and every frame after that
/// until the reply lands — otherwise a suppression that only covered the
/// click frame would be a silent no-op, since that frame is never the one
/// in the screenshot.
///
/// `requested_this_frame` is checked before `event_landed` so a new
/// request that happens to land in the same frame as an old capture's
/// reply keeps the guard set (a fresh capture is now in flight) rather
/// than clearing it.
fn screenshot_capture_guard(current: bool, requested_this_frame: bool, event_landed: bool) -> bool {
    if requested_this_frame {
        true
    } else if event_landed {
        false
    } else {
        current
    }
}

/// Issue #156: how many consecutive frames `screenshot_capturing` may stay
/// latched with no landed `Event::Screenshot` reply before `screenshot_
/// capture_timed_out` gives up on it and `OverlayApp::ui` forces the guard
/// closed anyway.
///
/// The reply can be silently dropped rather than merely slow: egui-wgpu's
/// `Painter::paint_and_update_textures` has several early-return paths — a
/// failed surface recreate, or its `render_state`/`surface_state` being
/// `None` — that return before `capture_data` ever reaches `read_screen_
/// rgba`, so the queued screenshot is dropped and no `Event::Screenshot` is
/// ever pushed. Without a bound, `screenshot_capture_guard` alone would
/// leave `screenshot_capturing` latched `true` for the rest of the
/// process, permanently suppressing the toggle cluster's hover fill and
/// tooltip on both buttons (they share the one flag) with no error and no
/// recovery short of restarting the app.
///
/// At the app's ~10Hz repaint cadence (`ctx.request_repaint_after` in
/// `OverlayApp::ui`) this is about 2 seconds — comfortably longer than any
/// real `ViewportCommand::Screenshot` round trip, short enough that a
/// dropped reply doesn't leave the suppression visible for long.
const SCREENSHOT_CAPTURE_TIMEOUT_FRAMES: u32 = 20;

/// Issue #156: true once `screenshot_capture_frames_waited` has reached
/// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES` — see that constant's doc comment
/// for why the `Event::Screenshot` reply can be silently dropped and never
/// arrive at all. `OverlayApp::ui` feeds this straight into `screenshot_
/// capture_guard` as `event_landed`, so a timed-out wait clears the guard
/// through the exact same pure transition a real reply does, rather than a
/// second, separately-maintained clearing path.
fn screenshot_capture_timed_out(frames_waited: u32) -> bool {
    frames_waited >= SCREENSHOT_CAPTURE_TIMEOUT_FRAMES
}

/// Issue #156: the frame-count half of the timeout fallback — advances
/// `OverlayApp::screenshot_capture_frames_waited` alongside `screenshot_
/// capture_guard`, sharing the same two inputs so the two pure functions
/// can never fall out of step with each other.
///
/// Resets to `0` the instant a new request fires or a reply (real or
/// timed-out) lands — both already reset `screenshot_capturing` itself —
/// and increments on every other frame, i.e. every frame the wait for the
/// reply continues.
fn advance_screenshot_capture_wait(
    frames_waited: u32,
    requested_this_frame: bool,
    event_landed: bool,
) -> u32 {
    if requested_this_frame || event_landed {
        0
    } else {
        frames_waited + 1
    }
}

/// The Share round trip's per-frame sequencing: collect any `Event::
/// Screenshot` images that landed this frame, decide (via
/// `take_pending_screenshot_bound`) what row bound to crop them to and
/// what `pending_screenshot_bound` must hold afterward, and hand each
/// cropped image to `write`.
///
/// Extracted out of `OverlayApp::ui` — mirroring this file's existing
/// pure-helper-for-testability convention (see `handle_screenshot_events`)
/// — because this sequencing was previously exercised only by driving the
/// whole `ui()` method, which is infeasible to test directly: `eframe::
/// Frame` has only `pub(crate)` fields and no public constructor. This
/// logic never actually touches `_frame`, though — it needs only `ctx` and
/// `pending_screenshot_bound`, both of which a bare `egui::Context::
/// default()` and a plain `&mut Option<f32>` stand in for in a test, no
/// GPU or window required. `write` is the same seam `handle_screenshot_
/// events` uses, so a test can assert on what would have been written
/// without a real clipboard.
///
/// The screenshot images are collected first, outside `handle_screenshot_
/// events`'s `ctx.input` borrow, so the pending-bound take (which needs
/// `&mut pending_screenshot_bound`) and the crop/write loop can happen
/// afterward without a borrow conflict.
fn handle_share_screenshot(
    ctx: &egui::Context,
    pending_screenshot_bound: &mut Option<f32>,
    mut write: impl FnMut(std::sync::Arc<egui::ColorImage>),
) {
    let mut screenshot_images: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
    handle_screenshot_events(ctx, |image| screenshot_images.push(image));
    let event_landed = !screenshot_images.is_empty();
    let (rows_bottom_y, new_pending_screenshot_bound) =
        take_pending_screenshot_bound(*pending_screenshot_bound, event_landed);
    *pending_screenshot_bound = new_pending_screenshot_bound;
    if event_landed {
        let pixels_per_point = ctx.pixels_per_point();
        for image in &screenshot_images {
            let cropped = crop_screenshot_to_rows(image, rows_bottom_y, pixels_per_point);
            write(cropped);
        }
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
// affordance, not a collapse-state indicator.

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
fn title_toggle_pill_rect(title_row: egui::Rect, height: f32) -> egui::Rect {
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

/// Fill of the timer pill — the source's `#1aaaaaaa`, a light gray at very
/// low alpha. Deliberately not `PILL_FILL`: the duration is the stat row's
/// lead readout and its capsule sits a shade lighter than the two value
/// pills beside it, which is what lets the eye separate "how long" from
/// "how much" without a divider between them.
const TIMER_PILL_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0xAA, 0xAA, 0xAA, 0x11);

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
/// because issue #49's death counter and issue #59's timer readout need
/// the same layout with different sizes, colors, glyph sides and corner
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
    /// Icon before the value instead of after it. Every header pill —
    /// timer, DPS and damage alike — reads value-then-icon, matching the
    /// reference render's `02:39 ⏱ | 188.0M/s ☁ | 30.10B ♡`; only issue #49's
    /// per-row death counter reads icon-then-value (skull, then count).
    icon_first: bool,
    /// Per-corner radius. Every pill is a full oval — all four corners at
    /// half the button row's height, never a flattened pair. The timer used
    /// to be a half-pill (`CornerRadius="0 13 13 0"`, welded to the panel's
    /// left border), which is the shape issue #91 fixed.
    corner_radius: egui::CornerRadius,
    /// Fill behind the pill. The timer's (`TIMER_PILL_FILL`) is a shade
    /// lighter than the value pills' `PILL_FILL`.
    fill: egui::Color32,
    /// Optional 1pt outline. No pill has one: the header's timer, DPS and
    /// damage ovals and the per-row counter are all fill-only. The timer
    /// carried the source's hairline `#2fff` border until issue #91 — ringed
    /// among three otherwise bare capsules, it read as an outlined odd one
    /// out rather than as the row's lead readout, and its lighter
    /// `TIMER_PILL_FILL` carries that distinction alone now. Kept as a field
    /// because the chrome is per-call-site and a stroked pill elsewhere
    /// stays a one-line change.
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
    fn timer(value: &'a str, icon: Option<egui::TextureId>) -> Self {
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
/// a live `egui::Ui` — same reasoning as `icon_slots`.
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

/// Header title text (issue #9 slice 2; gated to boss fights by issue #42;
/// dungeon-final-boss precedence by issue #125; live-boss-first by issue
/// #131).
///
/// Precedence, highest first:
///
/// 1. The *live* boss, when the currently-selected target is a genuinely
///    recognized boss (`is_boss`): its resolved name, or `Monster #{id}`
///    when it's a recognized boss whose name didn't resolve (the two
///    vendored lists aren't guaranteed to agree — see
///    `EncounterInfo::is_boss`). This outranks `scene_boss_name` —
///    inverted from issue #125's original precedence — because a raid can
///    string several *different* final bosses together in one instance
///    (repo owner, issue #131), so once a second or third raid boss is
///    actually engaged, showing anything but the boss currently being
///    fought would be actively wrong. A single-final-boss dungeon is
///    unaffected: nothing else is ever `is_boss` there once the run's
///    target is the curated boss itself.
/// 2. else `scene_boss_name` — the current dungeon's final boss from the
///    curated `tables::SCENE_FINAL_BOSSES` (issue #201; learned at runtime
///    before that, issue #125/#131). This is what covers both "just walked
///    in, nothing engaged yet" (`boss_monster_id` is still `None`) and the
///    issue #125 case: `recompute_boss` selected a non-boss mid-dungeon
///    mech or add as `boss_uid` (`is_boss` false), so branch 1 doesn't
///    apply, but the header still shouldn't go blank or fall through to
///    "No target" when the dungeon's real boss is already known. Only
///    curated single-boss dungeons have an answer here; every other scene
///    falls through.
/// 3. else, with nothing engaged at all in a scene that offers a *choice*
///    of bosses (`multi_boss_scene`, issue #150): "Select a boss". Every
///    raid lets the party pick one of three bosses, and picking again after
///    a win or a wipe never leaves the scene — so this is the caption for
///    the whole time the party is standing in the instance with nothing
///    selected. Naming a boss there (branch 2 is suppressed for these
///    scenes precisely because the remembered one is a wrong guess two
///    times out of three) or calling it "No target" would both misdescribe
///    the moment. Branch 1 takes back over the instant *any* recognized
///    boss is engaged.
/// 4. else the pre-#125 fallback: blank for a non-boss pull with nothing
///    remembered, "No target" when nothing has been hit yet at all.
///
/// Always returns something (never omits the line) so `draw_header` can
/// render it unconditionally and the header's height never jitters between
/// frames depending on whether a target — or a name for it — is known.
///
/// "No target" is kept for the genuinely-empty-encounter case (no target at
/// all), but a non-boss pull with no remembered dungeon boss is a real
/// target we're deliberately not naming — showing `Monster #{id}` there was
/// dropped rather than kept as the non-boss fallback, since a raw id would
/// read as an unresolved boss name rather than the intentional omission it
/// actually is (the reference meter only names boss fights; see
/// `tables::is_boss_monster`). A *recognized boss* with no resolved name is
/// different: it's a real boss fight, and an empty header would be
/// indistinguishable from a trash pull — so that case still falls back to
/// the raw id.
///
/// The issue #131 limitation this used to carry — on entry to a multi-boss
/// raid, `scene_boss_name` naming whichever boss was latched last rather
/// than the one the party will actually fight — is what issue #150 fixed,
/// by having `Meter::snapshot` suppress that fallback for such a scene and
/// set `multi_boss_scene` instead (issue #201's curated table is
/// single-boss dungeons only, so it cannot reintroduce it either). Branch 3
/// is the resulting caption.
///
/// While a finished fight is being held on screen, every field here
/// describes *that* fight rather than live state (issue #152), so nothing
/// in this precedence has to know about zoning: the header keeps naming the
/// boss whose frozen numbers are on the rows below it.
///
/// Also called by `pipeline::record_fight_end` so a saved encounter stores
/// the *same* label the live header showed (issue #39, spec DECISION D2).
pub(crate) fn encounter_title(e: &EncounterInfo) -> String {
    if e.is_boss {
        // `is_boss` is only ever true alongside a `Some` `boss_monster_id`
        // (see `Meter::snapshot`), so the `None` arm here is unreachable in
        // practice — kept only so this stays a total match rather than an
        // `unwrap`.
        return match e.boss_monster_id {
            Some(id) => e
                .boss_name
                .map(str::to_string)
                .unwrap_or_else(|| format!("Monster #{id}")),
            None => String::new(),
        };
    }
    if let Some(name) = e.scene_boss_name {
        return name.to_string();
    }
    match e.boss_monster_id {
        None if e.multi_boss_scene => "Select a boss".to_string(),
        None => "No target".to_string(),
        Some(_) => String::new(),
    }
}

/// Header subtitle text (issue #9 slice 2): the scene name when known, else
/// its raw scene id, else `None` — `draw_header` paints the subtitle row
/// blank in that case. The row's space is reserved either way (issue #91,
/// `header_text_band_height`); only its ink is conditional.
///
/// Also called by `pipeline::record_fight_end` so a saved encounter stores
/// the *same* label the live header showed (issue #39, spec DECISION D2).
pub(crate) fn encounter_subtitle(e: &EncounterInfo) -> Option<String> {
    match (e.scene_name, e.scene_id) {
        (Some(name), _) => Some(name.to_string()),
        (None, Some(id)) => Some(format!("Scene #{id}")),
        (None, None) => None,
    }
}

/// Height of the header's title line, reused by both `draw_header`'s
/// drag-band sizing and `default_inner_height` so they can't drift apart —
/// the same pattern `ROW_HEIGHT` follows for player rows.
/// Issue #91 raised this from `20.0`: measured against
/// `docs/reference/new-shinra-ex.webp`, our boss name sat with barely a
/// point of air under its descenders, while the reference leaves the name a
/// visibly taller line box.
const TITLE_LINE_HEIGHT: f32 = 22.0;

/// White the title line is painted in — the source's inherited `White`
/// title foreground, deliberately not `ui.visuals().text_color()` (the
/// theme's default, dimmer body-text white) since the title needs to read
/// as the visually heaviest element in the header. `draw_title_line` is its
/// only user: the header stat pills, which once shared it, are painted a
/// step dimmer in `PILL_VALUE_COLOR` so the title still outweighs them.
const TITLE_TEXT_COLOR: egui::Color32 = egui::Color32::WHITE;

/// Height of the header's subtitle line, always reserved by the header band
/// whether or not there is an area name to paint into it (issue #91, see
/// `header_text_band_height`). Still not part of `default_inner_height`,
/// which keeps the opening size it was measured at (see its doc).
///
/// `TITLE_LINE_HEIGHT (22) + ITEM_SPACING_Y (2) + SUBTITLE_LINE_HEIGHT (16)
/// == 40.0`. Issue #91 grew both line heights off the source's original
/// `Height="36"` grid (`20 + 2 + 14`): pixel-measured against
/// `docs/reference/new-shinra-ex.webp`, the reference's area name clears its
/// own descenders and the separator above it with more room than a 14pt line
/// box leaves.
const SUBTITLE_LINE_HEIGHT: f32 = 16.0;

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
    header_text_rect_reserving(row, HEADER_RIGHT_CONTROL_WIDTH)
}

/// The sub-rect the *title* row's text may paint into (issue #185): the same
/// geometry as `header_text_rect`, but reserving `TITLE_RIGHT_CONTROLS_
/// WIDTH` — the chevron strip *plus* the toggle pill that now sits left of
/// it — instead of the chevron strip alone. The subtitle row keeps
/// `header_text_rect`, since the pill is on the title row only.
fn title_text_rect(row: egui::Rect) -> egui::Rect {
    header_text_rect_reserving(row, TITLE_RIGHT_CONTROLS_WIDTH)
}

/// The shared body of `header_text_rect` and `title_text_rect`: the two
/// differ only in how much of the row's right end is reserved for controls,
/// and every degradation rule (never inverted, clamped against the left
/// edge) is identical, so it is spelled once here rather than twice.
fn header_text_rect_reserving(row: egui::Rect, right_reserve: f32) -> egui::Rect {
    let left = row.left() + HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;
    let right = (row.right() - right_reserve).max(left);
    egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()))
}

/// Gap between the header's text band (title + subtitle) and the
/// stat-pill row below it. Its own constant rather than the `ITEM_SPACING_Y`
/// (2.0) every other adjacent pair pays, because the stat row has to clear
/// the subtitle's *descenders* the way the reference does: measured on
/// `docs/reference/new-shinra-ex.webp`, the reference leaves 14px between the
/// subtitle's ink and the stat row's ink, where ours left 8px and the pills
/// crowded the area name. The extra 4pt here is what buys the difference
/// without inflating every other gap in the window.
///
/// `draw_header` applies it explicitly (`ui.add_space`) *and* budgets it in
/// `header_band_height`, so the drag band, the wash and the painted rows all
/// stay derived from this one number.
const HEADER_STAT_ROW_GAP: f32 = 6.0;

/// How far the stat row (timer, DPS, damage, toggle cluster) is inset from
/// the panel's left content edge. The timer used to sit flush against it, so
/// its capsule was cropped by the window border and drawn as a half-pill to
/// match; inset, it can wear a whole oval (see `StatPill::timer`).
///
/// Exactly the title column's own indent, not a margin of its own:
/// `header_text_rect` starts the boss name and area name at this same sum,
/// so the timer reads as sitting *under* the title rather than tucked into
/// the gutter beside it.
///
/// It is also what keeps the gutter emblem off the timer. That mark's box
/// ends at `HEADER_EMBLEM_LEFT_BLEED + HEADER_EMBLEM_SIZE ==
/// HEADER_GUTTER_WIDTH` and hangs 14pt into this row's vertical span, so
/// this sum leaves `HEADER_TEXT_PAD_X` of daylight between the mark's right
/// edge and the row's leftmost ink — which is why the mark no longer has to
/// be clipped to the text band (and have its bottom corner sliced off) to
/// stay clear of the readout. See `header_emblem_rect`.
const HEADER_STAT_ROW_INSET_X: f32 = HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;

/// Height of `draw_header`'s drag band: the title line, the subtitle line,
/// and the button row (`button_row_height`, egui's `interact_size.y`), plus
/// the gaps egui's vertical layout stacks between them — `ITEM_SPACING_Y`
/// between the title and subtitle rows, and `HEADER_STAT_ROW_GAP` above the
/// stat-pill row. Extracted from `draw_header` so it is unit-testable
/// without a live `egui::Ui`.
///
/// A constant `68.0` at the real `BUTTON_ROW_HEIGHT`, with no dependence on
/// whether an area name is known — see `header_text_band_height`.
fn header_band_height(button_row_height: f32) -> f32 {
    header_text_band_height() + HEADER_STAT_ROW_GAP + button_row_height
}

/// Issue #158: the panel-top-relative y where the first player row
/// actually begins — which is *not* `band_height`. `OverlayApp::ui` puts a
/// `ui.separator()` (`SEPARATOR_HEIGHT`, egui's own fixed 6.0) between the
/// header and the row list, and egui's vertical layout pays its ordinary
/// `ITEM_SPACING_Y` gap before that separator, same as between any other
/// two consecutive widgets in the panel. So the band's own bottom edge is
/// 8pt short of where the rows start; this is the single function both
/// `default_inner_height` (the window's default open height) and the
/// header wash (`draw_header`'s `wash_height`) derive the true offset from,
/// so the two can never drift back out of sync the way `band_height` alone
/// did.
fn first_player_row_top_offset(band_height: f32) -> f32 {
    band_height + ITEM_SPACING_Y + SEPARATOR_HEIGHT
}

/// Height of the header's *text* rows alone: the title line, the gap, and
/// the subtitle line under it (issue #91's `22 + 2 + 16`, grown from the
/// source's `Height="36"` grid).
///
/// Unconditional. The subtitle's line and gap are reserved whether or not a
/// scene is known, so the header is a fixed-height band: the app's idle
/// "No target" state used to skip them, which collapsed the band from 68 to
/// 50 and jumped the whole stat-pill row 18pt up the window the moment the
/// area name arrived or went away. `draw_header` renders the subtitle row
/// empty rather than omitting it (see
/// `a_missing_area_name_does_not_collapse_the_header_or_lift_the_stat_row`).
///
/// This, not the whole drag band, is the block the left gutter belongs to:
/// `header_text_rect` indents these rows by `HEADER_GUTTER_WIDTH` to leave
/// room for the emblem, which is why the emblem's box is *centered* on this
/// height (issue #75) rather than on the band.
///
/// Centered on, no longer clipped to. Issue #75 did both; the clip cut the
/// mark's bottom corner off, and issue #91 replaced it with horizontal
/// clearance instead (`HEADER_STAT_ROW_INSET_X`, which now starts the stat
/// row right of the emblem rather than under it). `draw_header` clips the
/// mark to the whole band.
///
/// The background wash is deliberately *not* bounded by this either (issue
/// #91): it spans the whole `header_band_height`, stat row included.
fn header_text_band_height() -> f32 {
    TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT
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
    // `title_text_rect`, not `header_text_rect`: the title row reserves the
    // toggle pill's width as well as the chevron's (issue #185).
    let rect = title_text_rect(row);
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
/// left edge by `Margin="-26 0 0 -8"`, so only its right two-thirds are ever
/// on screen. Vertically it is centered on the header's *text* band but
/// clipped to the whole header band — see `header_emblem_rect`.
const HEADER_EMBLEM_SIZE: f32 = 60.0;
/// The `Margin`'s left component: the emblem hangs 26pt off the left edge of
/// the header rows, so only its right `60 - 26 = 34`pt — one
/// `HEADER_GUTTER_WIDTH` — is ever on screen.
const HEADER_EMBLEM_LEFT_BLEED: f32 = -26.0;
/// The `Margin`'s bottom component (`-8`): a *negative* bottom margin, which
/// in WPF adds to the height the emblem is centered in rather than moving
/// it. With the source's 36pt header grid that gives `(36 + 8 - 60)/2 = -8`,
/// i.e. a top edge 8pt above the grid. Our text band is 40pt rather than 36
/// (issue #91 grew the two line heights), so `header_emblem_rect` recomputes
/// the centering from the text band it is actually given instead of baking
/// the source's one case in.
const HEADER_EMBLEM_BOTTOM_BLEED: f32 = 8.0;
/// `Fill="SlateGray"`.
const HEADER_EMBLEM_COLOR: egui::Color32 = egui::Color32::from_rgb(0x70, 0x80, 0x90);

/// Where the header emblem's 60x60 box sits: bled off the left of the title
/// row `row`, and vertically centered on the header's text band
/// (`text_band_height`, plus the source's negative bottom margin) that
/// starts at that row's top. Pure geometry, so the negative-margin bleed is
/// unit-testable without a painter.
///
/// **The off-centre placement here is deliberate — do not "fix" it.** It has
/// been squared up to the band once (issue #91) and reverted once.
/// `emblem.png` is a 512x512 canvas holding one perfect diamond whose
/// corners are the midpoints of the canvas' edges, so a box of exactly
/// `header_band_height` *does* make the diamond symmetric about the band —
/// and that is not what this mark is. It decorates the title and area-name
/// rows, the ones `header_text_rect` indents by `HEADER_GUTTER_WIDTH` to
/// make room for it, so it is centered on those rows (plus the source's
/// negative bottom margin) and rides high in the band: top edge 6pt above
/// the panel, bottom edge 14pt below the text band. Both numbers are the
/// design.
///
/// What is *no longer* true is that a clip is what keeps its lower blade
/// off the timer. Issue #75 clipped this to the text band for that, and the
/// clip landed at 40 while the box reaches 54 — it sliced the diamond's
/// bottom corner clean off, which is the bug that reading closes. The stat
/// row moved right instead
/// (`HEADER_STAT_ROW_INSET_X == HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X`),
/// clearing the emblem's right edge at `HEADER_GUTTER_WIDTH` by
/// `HEADER_TEXT_PAD_X`; that horizontal daylight is the whole separation
/// now, and `draw_header` clips the mark to the header band, which cuts
/// nothing off the bottom. The mark's *top* corner is still cut — by the
/// panel's own top edge, since the box starts above it. That is the
/// asymmetry, and it stays.
///
/// This bounds the *gutter* mark only. The background wash
/// (`header_wash_rect` / `draw_header_wash`) is a different rect and also
/// spans the whole header band, stat-pill row included.
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
///
/// Issue #81 replaced a fixed `98.0`pt run — taller than the drag band
/// itself, so its tail bled into the player rows — with a height derived
/// from the content. Issue #91 settles which content: the whole header band
/// (`header_band_height`), stat-pill row included. The gradient and the
/// oversized emblem share one rect, so both now run the full band. Issue
/// #91 believed that made both flush with the first player row; issue #158
/// found the band's own bottom edge is actually 8pt short of it (the
/// `ui.separator()` between the header and the rows, plus the layout's
/// `ITEM_SPACING_Y` gap before it, are both outside the band) and extended
/// the wash past the band to `first_player_row_top_offset` so it now really
/// does stop flush with the first player row. No fixed constant is left to
/// drift out of sync with the content it sits behind.
/// Inset from the panel's edges the wash is painted at, so its square
/// corners never poke past the panel's own `PANEL_CORNER_RADIUS`-rounded,
/// `PANEL_BORDER_WIDTH`-thick border.
const HEADER_WASH_INSET: f32 = 1.0;
/// Alpha at the wash gradient's brightest (top-left) stop — `Opacity=".5"`.
const HEADER_WASH_TOP_ALPHA: u8 = 0x50;
/// Side of the wash's oversized `Svg.HPBar` box, in points — the same emblem
/// the gutter draws at `HEADER_EMBLEM_SIZE`, blown up as wallpaper.
const HEADER_WASH_EMBLEM_SIZE: f32 = 200.0;
/// How far the wash emblem's right edge overhangs the wash's own right edge,
/// in points: the source right-aligns the wash `Svg.HPBar` with a `-25` right
/// margin, so its last 25pt hang off the panel and the wash's clip rect cuts
/// them away — the mirror of the gutter emblem's `HEADER_EMBLEM_LEFT_BLEED`.
const HEADER_WASH_EMBLEM_BLEED: f32 = 25.0;
/// `Opacity=".05"` on a SlateGray fill.
const HEADER_WASH_EMBLEM_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x70, 0x80, 0x90, 13);

/// Where the wash panel sits for a central panel of `panel`: inset from the
/// panel's left, top and right edges by `HEADER_WASH_INSET`, and running down
/// `height` points rather than to the panel's bottom. Pure geometry, so the
/// inset is unit-testable without a painter — the same factoring as
/// `header_emblem_rect`. `height` is the caller's to pick (`draw_header`
/// passes `header_band_height - HEADER_WASH_INSET`, issue #91) rather than a
/// fixed constant here, so the wash can never outgrow the band it decorates.
fn header_wash_rect(panel: egui::Rect, height: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        panel.min + egui::Vec2::splat(HEADER_WASH_INSET),
        egui::vec2(panel.width() - 2.0 * HEADER_WASH_INSET, height),
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
/// (not the drag band); `height` (issue #158, `first_player_row_top_offset`
/// of `header_band_height` less `HEADER_WASH_INSET`) is what actually
/// bounds the wash — the whole header band plus the separator gap below
/// it, stopping exactly where the first player row begins
/// (`wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row`).
///
/// The source rounds the wash's top corners (`CornerRadius="7 7 0 0"`); egui
/// cannot clip to a rounded rect this cheaply, so the wash keeps square
/// corners — at alpha `0x50` under the panel's own 8pt-rounded, 1pt border
/// the difference is sub-pixel.
fn draw_header_wash(ui: &egui::Ui, panel: egui::Rect, icons: &Icons, height: f32) {
    let wash_rect = header_wash_rect(panel, height);
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

/// Paints the header's subtitle line (scene name/id), dimmed. Always
/// called, with empty text when `encounter_subtitle` returned `None`
/// (issue #91): the row's height is part of the header's fixed band, so it
/// is reserved — and rendered blank, with no placeholder — rather than
/// skipped, which would let the stat row below it ride up.
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
/// helper per item, since egui's own `menu_button`/`CollapsingHeader`
/// already give every item free open/close state management.
///
/// Order matches the spec: a Columns disclosure section (issue #13's stat
/// column toggles, unchanged in behavior — just relocated), a separator,
/// the Opacity slider (issue #166), a separator, Minimize to tray, a
/// separator, Reset to defaults (issue #203) and Export logs (issue #220),
/// a separator, Check for updates and its result line (issue #171), a
/// separator, then Close. "Forget learned bosses" (issue #131) sat between
/// the Opacity slider and Minimize until issue #201 replaced the runtime
/// scene -> boss learning it reset with a curated static table
/// (`bpsr_meter::tables::SCENE_FINAL_BOSSES`), leaving nothing to forget.
/// Reset used to be the first item here but moved
/// into the header's toggle cluster (issue #82; see `toggle_cluster`),
/// leaving this menu with no reset trigger of its own.
/// Collapse/Expand (issue #54's collapse-to-header) used to be the item
/// between Columns and Minimize; it was removed outright rather than
/// reworked, since the chevron no longer indicates a collapse state to
/// toggle (see `menu_chevron`).
///
/// Columns renders its checkboxes inline behind a click-to-expand
/// `CollapsingHeader` rather than the hover `SubMenu` flyout it used to be
/// (issue #93). The flyout had two compounding problems in this egui
/// version: it opened purely on hover with no dwell delay, and its own
/// `CloseOnClickOutside` config (needed for the reason below) meant leaving
/// the hover didn't close it either — an accidental hover trapped the
/// pointer until an explicit outside click. It also requested
/// `RectAlign::RIGHT_START` but could fall back to the mirrored
/// `LEFT_START` near a screen edge, opening back over the trigger instead
/// of beside it — exactly what this screen-edge overlay routinely sits
/// near. A disclosure section has no second popup layer to mistime or
/// mis-position, so neither problem is reachable.
///
/// Built from `collapsing_header::CollapsingState::show_header` rather than
/// the plain `egui::CollapsingHeader::new(...).show(...)` shorthand so the
/// header can carry `ToolbarIcon::Settings` beside the "Columns" label —
/// otherwise this item would have no icon call site while every other item
/// in the menu (Close) still does, an inconsistency review flagged on this
/// PR.
///
/// Losing the submenu also loses the `close_behavior` it carried on its own:
/// the checkboxes are now direct children of the *root* popup, so
/// `draw_header`'s `close_behavior(CloseOnClickOutside)` on that root popup
/// does the checkbox-safe job the submenu's own config used to — a
/// checkbox click no longer dismisses the whole dropdown. Minimize/Close
/// call `ui.close()` themselves so they still dismiss it on click, matching
/// the root's old `CloseOnClick` default for those two items.
// Issue #39: same reasoning as `draw_header`'s identical allow just above —
// one more history-view parameter tips this over clippy's default limit.
#[allow(clippy::too_many_arguments)]
fn draw_header_menu(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    // Issue #171: the manual "Check for updates" item's in-flight/last-
    // result state — see `UpdateCheckState`'s doc comment.
    update_check: &mut UpdateCheckState,
    // Issue #220: the reply channel each "Export logs" click's spawned
    // thread sends its outcome back over — see `LogExportOutcome`.
    tx_log_export: &Sender<LogExportOutcome>,
) {
    let SettingsHandle {
        settings,
        tx_settings,
    } = settings;

    let columns_id = ui.make_persistent_id("header_menu_columns");
    let mut columns_state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ctx, columns_id, false);
    // Built from `CollapsingState` directly, rather than the plain
    // `egui::CollapsingHeader::new(...).show(...)` shorthand this replaced,
    // so the header can carry `ToolbarIcon::Settings` beside the "Columns"
    // label. `show_toggle_button`'s own click target is only the small
    // disclosure-arrow box, unlike `CollapsingHeader::show`'s whole clickable
    // row — so the icon+label response is unioned and toggled by hand below,
    // to keep the same full-row click target rather than shrinking it down
    // to the arrow alone.
    let header_response = ui.horizontal(|ui| {
        columns_state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
        // A bare `ui.label` only senses hover, never clicks — `Sense::click()`
        // is what actually lets the union below toggle on a label/icon click
        // rather than doing nothing.
        let mut header_response = ui.add(egui::Label::new("Columns").sense(egui::Sense::click()));
        if let Some(handle) = icons.toolbar.get(ToolbarIcon::Settings) {
            // Same reasoning as the label above: `toolbar_icon_image` builds
            // a plain `egui::Image`, hover-sensing only by default, so it
            // needs an explicit click sense to actually contribute to the
            // unioned header-row click target.
            header_response |= ui.add(toolbar_icon_image(handle).sense(egui::Sense::click()));
        }
        header_response
    });
    if header_response.inner.clicked() {
        columns_state.toggle(ui);
    }
    columns_state.show_body_indented(&header_response.response, ui, |ui| {
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

    // Issue #166: its own labelled section (not nested inside the Columns
    // disclosure above) since it toggles a single overlay-wide value rather
    // than a per-column list — a `CollapsingHeader` would just be an extra
    // click for something that's already only one control. Placed in the
    // header's settings dropdown, alongside Columns, rather than the
    // header's toggle-cluster pill (a separate, unrelated header surface —
    // see `toggle_cluster` — that a different issue is changing in
    // parallel).
    ui.label("Opacity");
    // Issue #182: the rail spans the full 0%-100%, floor included. Nothing
    // here has to guard the bottom end — `Settings::OPACITY_MIN` documents
    // why a fully transparent backdrop stays recoverable.
    let mut opacity = settings.opacity;
    let opacity_response = ui.add(
        egui::Slider::new(&mut opacity, Settings::OPACITY_MIN..=Settings::OPACITY_MAX)
            .show_value(false),
    );
    if opacity_response.changed() {
        // Applied immediately (same frame): `draw_header_menu` mutates the
        // caller's `&mut Settings` in place, and `OverlayApp::ui` reads
        // `self.settings.opacity` fresh when it builds the panel `Frame`
        // right after `draw_header` returns — no extra repaint request
        // needed, unlike an async round trip such as the Share screenshot's.
        settings.set_opacity(opacity);
        // Same persistence path as the Columns checkboxes just above:
        // blocking file IO stays off this render thread, and a dropped
        // writer thread just leaves the in-memory value correct for the
        // rest of this session.
        let _ = tx_settings.send(settings.clone());
    }

    ui.separator();

    // Issue #53: this minimize goes to the notification area, not the
    // taskbar. `platform::install_tray`'s subclass intercepts the
    // `WM_SIZE`/`SIZE_MINIMIZED` this command produces, adds a tray icon
    // and hides the window, so no call-site change is needed here — but the
    // tray icon is now the *only* way back, so don't route this through
    // anything that bypasses a real minimize.
    if ui.button("Minimize to tray").clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ui.close();
    }

    ui.separator();

    // Issue #203: a UI-settings reset (window size + opacity), distinct
    // from the tray's own OS-level `TrayCommand::ResetWindow` (which
    // recenters/resizes back to the full 20-row raid default and never
    // touches opacity). This resizes to a 5-row sample instead, using the
    // same column set `default_inner_width` already sizes for, and puts
    // opacity back to `Settings::default_opacity()` through the same
    // `set_opacity` + `tx_settings` path the slider above uses.
    if ui.button("Reset to defaults").clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            default_inner_width(),
            reset_to_defaults_inner_height(),
        )));
        settings.set_opacity(Settings::default_opacity());
        let _ = tx_settings.send(settings.clone());
        ui.close();
    }

    // Issue #220: a user hitting a bug has no in-app way to hand over the
    // logs `logging::init` already writes for exactly this purpose. Never a
    // fixed or hidden path — the save dialog is what lets the user pick the
    // destination themselves, per the issue.
    //
    // The dialog itself is called inline, on this thread:
    // `platform::choose_log_export_path`'s own doc comment explains why a
    // modal the OS is already blocking on needs no thread. The copy that
    // follows is a different matter — up to two files of
    // `logging::MAX_LOG_BYTES` — so it goes to a spawned thread (PR #227
    // review), the "Check for updates" shape.
    if ui.button("Export logs").clicked() {
        if let Some(dest) =
            crate::platform::choose_log_export_path(crate::logging::EXPORT_DEFAULT_FILENAME)
        {
            start_log_export(dest, tx_log_export.clone());
        }
        ui.close();
    }

    ui.separator();

    // Issue #171: manual-only, per the issue — there is no automatic or
    // background check anywhere in this crate, only this button. The
    // request itself never touches this thread: clicking spawns a
    // dedicated `std::thread` that calls `update_check::check_for_update`
    // and reports back over a fresh `crossbeam_channel`, the same
    // one-shot-thread shape `settings::spawn_writer` uses for its own
    // (longer-lived) writer thread — see `UpdateCheckState`'s doc comment
    // for why the state lives on `OverlayApp` rather than here.
    //
    // The button stays enabled (and re-clickable) once a check is done,
    // both to let the user retry after a transient network error and to
    // let them re-check right before actually upgrading; it is only
    // disabled while one is already in flight, so a click can't pile up a
    // second thread racing the first to the same channel.
    let checking = matches!(update_check, UpdateCheckState::Checking { .. });
    let clicked_check_for_updates = ui
        .add_enabled(!checking, egui::Button::new("Check for updates"))
        .clicked();
    if clicked_check_for_updates {
        *update_check = start_update_check();
    }
    match update_check {
        UpdateCheckState::Idle => {}
        UpdateCheckState::Checking { .. } => {
            ui.label("Checking…");
        }
        UpdateCheckState::Done(Ok(CheckOutcome::UpToDate)) => {
            ui.label(format!("Up to date (v{})", env!("CARGO_PKG_VERSION")));
        }
        UpdateCheckState::Done(Ok(CheckOutcome::UpdateAvailable { tag, url })) => {
            ui.horizontal(|ui| {
                ui.label(format!("Update available: {tag}"));
                // Issue #171 scopes auto-download/apply out — this link to
                // the release's own GitHub page is the whole "get it"
                // affordance. `egui::OpenUrl` (what `hyperlink_to` sends
                // through `ctx.output_mut`) is what eframe's native
                // backend turns into an actual browser launch.
                ui.hyperlink_to("Download", url.as_str());
            });
        }
        UpdateCheckState::Done(Err(err)) => {
            ui.label(format!("Update check failed: {err}"));
        }
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
        ui.close();
    }
}

/// State of a manual "Check for updates" request from the header dropdown
/// (issue #171). Lives on `OverlayApp` (`update_check` field), not local to
/// `draw_header_menu`, for two reasons: the popup itself is only painted
/// while open (`egui::Popup::menu`'s `show` closure runs once per frame
/// *it* is open, not every app frame), so any state stored purely in its
/// closure's locals would be reset to nothing the next time it opens; and
/// `OverlayApp::poll_update_check` needs to drain the `Checking` channel
/// once per app frame regardless of whether the dropdown happens to be
/// open that frame, so a result that lands while it's closed is still
/// there — not dropped — the moment it's reopened.
#[derive(Debug, Default)]
enum UpdateCheckState {
    /// No request has been made yet this session, or this is a fresh
    /// `OverlayApp`.
    #[default]
    Idle,
    /// A request is in flight on a spawned thread; `rx` is that thread's
    /// reply channel, drained by `OverlayApp::poll_update_check`.
    Checking {
        rx: Receiver<Result<CheckOutcome, String>>,
    },
    /// The most recent request has resolved, successfully or not.
    Done(Result<CheckOutcome, String>),
}

/// Starts a manual update check (issue #171): spawns a one-shot
/// `std::thread` that calls `update_check::check_for_update` — the pure
/// decision logic plus, on Windows, the actual `platform::http_get` network
/// call — and sends its `Result` back over a fresh, unbounded
/// `crossbeam_channel`. Returns the `Checking` state `draw_header_menu`'s
/// click handler stores on `OverlayApp` so `poll_update_check` knows to
/// start draining it.
///
/// Never called from, and never blocks, the UI thread: the click handler
/// only starts the thread and returns immediately, the same shape
/// `settings::spawn_writer`'s doc comment describes for its own writer
/// thread (a persistent one, unlike this one-shot version, since a
/// settings write can happen many times a session and an update check is
/// only ever one click at a time).
fn start_update_check() -> UpdateCheckState {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("update-check".to_string())
        .spawn(move || {
            let _ = tx.send(update_check::check_for_update(env!("CARGO_PKG_VERSION")));
        })
        .expect("failed to spawn the update-check thread");
    UpdateCheckState::Checking { rx }
}

/// What one "Export logs" thread reports back (issue #220): the
/// destination it finished writing, or that destination plus why it
/// couldn't. The destination rides along on the failure too so
/// `OverlayApp::poll_log_export` can name it in the log line — by the time
/// a reply lands, the click that chose it is long gone.
type LogExportOutcome = Result<PathBuf, (PathBuf, String)>;

/// Starts a log export (issue #220, PR #227 review): spawns a one-shot
/// `std::thread` that bundles the log files into `dest` and sends the
/// outcome back over `tx`, a clone of `OverlayApp::tx_log_export` that
/// `poll_log_export` drains once a frame.
///
/// Off the frame thread because `logging::export_logs_to` copies up to two
/// files of `logging::MAX_LOG_BYTES` each — a ~10MB disk copy, which is
/// exactly the multi-frame stall the overlay must not take while the game
/// is running. Same one-shot shape as `start_update_check`, and for the
/// same reason.
///
/// Resolving the log path happens on the spawned thread as well: it is
/// `std::env::var` plus path joining, cheap either way, and keeping it
/// beside the copy leaves the click handler with nothing to do but hand
/// over the destination.
fn start_log_export(dest: PathBuf, tx: Sender<LogExportOutcome>) {
    std::thread::Builder::new()
        .name("export-logs".to_string())
        .spawn(move || {
            let (log_path, _warning) = crate::logging::log_file_path();
            let outcome = match crate::logging::export_logs_to(&log_path, &dest) {
                Ok(()) => Ok(dest),
                Err(err) => Err((dest, err.to_string())),
            };
            // A dropped receiver means the app is shutting down; the export
            // itself already happened, so there is nothing to report or
            // retry.
            let _ = tx.send(outcome);
        })
        .expect("failed to spawn the export-logs thread");
}

/// The "Export logs" reply channel the header tests below hand to
/// `draw_header`/`draw_header_menu`. None of them click that item, so
/// nothing is ever sent over it — it exists only to satisfy the parameter,
/// and its dropped `Receiver` matters to nobody for the same reason.
#[cfg(test)]
fn unused_log_export_sender() -> Sender<LogExportOutcome> {
    crossbeam_channel::unbounded().0
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

/// Issue #74: whether a gesture of `kind` ending in `viewport` should force
/// a DWM frame recompute (`platform::force_frame_recompute`). Only a resize
/// changes the window's size, and the frame that goes stale is the one
/// Win32 was never told about — a pure `Move` gesture ending has nothing to
/// recompute a frame for.
///
/// The `viewport` half is issue #218: `draw_skill_window` now drives this
/// same gesture code for every open breakdown child viewport, but
/// `force_frame_recompute` has exactly one window to aim at — the root
/// `HWND` cached at startup, because the `ui.rs` call sites only ever hold
/// an `egui::Context` and no per-viewport handle exists (see its doc
/// comment in `platform`). Firing it from a child would `SetWindowPos` the
/// *root* window over a resize the root never underwent: nothing for the
/// window that actually resized, and a stray call against the one `HWND`
/// the Snap blocker watches. So child viewports are left out — the same
/// shape of gap as the reposition exemption `draw_skill_window` documents
/// as root-HWND-only and inert there.
///
/// Pulled out of `drive_window_gesture` as a pure function, the same way
/// `row_bar_frac`/`share_bar_paints`/`column_anchors` extract pure decisions
/// out of `Ui`-dependent code elsewhere in this file, so this call-site
/// choice is unit-testable without a window.
fn gesture_end_needs_frame_recompute(kind: GestureKind, viewport: egui::ViewportId) -> bool {
    viewport == egui::ViewportId::ROOT && matches!(kind, GestureKind::Resize(_))
}

/// Issue #183: whether the header's drag band must refuse to start a window
/// move, because the overlay is pinned (`Settings::always_on_top`).
///
/// The pin toggle only ever sent `ViewportCommand::WindowLevel`, so a pinned
/// overlay stayed on top but could still be shoved anywhere on the desktop
/// by an accidental drag of its header — which is the opposite of what a
/// pushpin means to anyone who clicks it. Pinning now locks both.
///
/// Deliberately only `GestureKind::Move`: the eight `resize_zones` stay
/// live, because "pinned" is about the overlay staying put, not about
/// freezing its size, and a pinned overlay the user can't resize would be a
/// second surprise rather than a fix for the first.
///
/// Trivial enough to inline, spelled as a function anyway so the rule has
/// one name and one doc comment shared by the drag band and
/// `cancel_move_gesture_when_pinned` — and so it is unit-testable without a
/// live `egui::Context`, same as this module's other pure header helpers.
fn drag_locked_by_pin(always_on_top: bool) -> bool {
    always_on_top
}

/// Issue #183: ends an in-flight *move* gesture the moment the overlay is
/// pinned, so pinning mid-drag stops the window there instead of letting
/// the drag run to completion.
///
/// The pin button lives in the header's own title row, which means the
/// pointer that clicked it is necessarily still down over the drag band
/// that could have started a move on the way in. Gating only the gesture's
/// *start* (`drag_locked_by_pin` at the drag band) would therefore leave
/// exactly one gesture — the one already running — free to keep moving the
/// window after the user asked for it to be locked.
///
/// A resize in flight is left alone, for the same reason `drag_locked_by_
/// pin` only covers `Move`.
fn cancel_move_gesture_when_pinned(gesture: &mut WindowGesture, always_on_top: bool) {
    if drag_locked_by_pin(always_on_top) && gesture.kind() == Some(GestureKind::Move) {
        gesture.end();
    }
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
/// `min_size` is the floor a resize clamps at — always `MIN_INNER_SIZE` from
/// the one live call site, but a parameter (rather than reading the constant
/// directly) so `resized_window_rect`'s clamp is exercised against an
/// arbitrary floor in its own unit tests.
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
        if gesture_end_needs_frame_recompute(kind, ctx.viewport_id()) {
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
///
/// `id_salt` namespaces the eight handle ids (issue #218): the same function
/// now serves the root window and every open breakdown viewport, and both
/// are drawn from a root `Ui` whose own id is the same in each — without a
/// salt, two windows' north handles would be one widget.
fn draw_resize_handles(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    gesture: &mut WindowGesture,
    // `Debug` alongside `Hash` because egui's `Id::with` takes an
    // `AsIdSalt`, and that is `Hash + Debug` — a debug build records the
    // salt's `Debug` rendering in `id_source` so an id clash names the
    // widgets that collided. Not ours to drop.
    id_salt: impl std::hash::Hash + std::fmt::Debug,
) {
    // The viewport this `Ui` belongs to — the root window, or, inside
    // `show_viewport_immediate`'s callback, the child. Either way it is the
    // rect `Ui::max_rect` was built from (egui's `root_ui`).
    let window = ctx.input(|i| i.viewport_rect());
    // `ResizeDirection` is not `Hash`, so the zone's position in the array is
    // what keeps the eight ids distinct.
    for (index, (zone, direction, cursor)) in resize_zones(window).into_iter().enumerate() {
        let handle = ui.interact(
            zone,
            ui.id().with((&id_salt, "resize", index)),
            egui::Sense::drag(),
        );
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
fn row_content_width(viewport_width: f32, stat_columns_total: f32) -> f32 {
    let floor_width = stat_columns_total * MIN_COLUMN_SCALE + COLUMN_RIGHT_MARGIN;
    viewport_width.max(floor_width)
}

/// Returns the `ScrollArea`'s reported content size — larger than the
/// viewport on whichever axis actually needed to scroll this frame — purely
/// so tests can observe that without reaching into egui's persisted scroll
/// state; `OverlayApp::ui` (the only production caller) ignores it.
fn draw_rows(
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
fn column_clip_rect(rect: egui::Rect, anchor: f32, width: f32) -> egui::Rect {
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
struct RowLayout<'a> {
    kinds: &'a [ColumnKind],
    columns: &'a [StatColumn],
    anchors: &'a [f32],
    /// Issue #168: `draw_row` needs the live `Settings` (not just `kinds`,
    /// which now excludes `AbilityScore`/`SeasonStrength` — see
    /// `Settings::stat_columns`) to compose the name-suffix text via
    /// `name_suffix`. Bundled into `RowLayout` alongside the three fields
    /// above for the same reason they are: keeps `draw_row`'s own argument
    /// count under clippy's limit rather than adding a fourth loose
    /// parameter.
    settings: &'a Settings,
}

fn draw_row(
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

/// Issue #97: floor a positive-damage row's bar fraction is clamped up to,
/// so a row far below the top row (e.g. a support/healer next to a burst
/// DPS class) still paints a noticeably visible sliver instead of reading
/// as if it did zero damage. 3% sits in the middle of the sane 2-5% range:
/// at the row bar's actual width (`share_bar_rect`'s 300pt test fixture)
/// that is a solid ~9px sliver — clearly present — while staying far below
/// any real double-digit share, so it never overstates a near-zero
/// contributor as roughly comparable to a genuine one.
const ROW_BAR_MIN_FRAC: f32 = 0.03;

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
fn row_bar_frac(damage: i64, top_damage: i64) -> f32 {
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

/// Square side of the per-row class icon (issue #9). The source's `Path
/// 18x18` was the original target; issue #187 bumps this a couple of
/// points past that on purpose, since the icon read too small in practice
/// — `ICON_MARGIN` is left unchanged, so the gutter math below no longer
/// centers this exactly in a 25px column, it just adds up.
const ICON_SIZE: f32 = 20.0;

/// Gap on both sides of the icon: between the row's left edge and the icon,
/// and between the icon and the Imagine gutter that follows it. `3.5` was
/// originally chosen so the class-icon portion of `ICON_GUTTER_WIDTH`
/// landed exactly on the source's 18px glyph centered in a fixed 25px
/// `SharedSizeGroup="p0"` column; issue #187 grew `ICON_SIZE` past 18
/// without touching this margin, so that exact 25px alignment no longer
/// holds — `25.0` was what `ICON_GUTTER_WIDTH` would have reverted to had
/// `IMAGINE_GUTTER_WIDTH` been deleted (D4's takedown) before issue #187.
const ICON_MARGIN: f32 = 3.5;

/// Class icon tint (source `Fill="#ddd"`).
const CLASS_ICON_TINT: egui::Color32 = egui::Color32::from_rgb(0xDD, 0xDD, 0xDD);

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Square side of each Imagine slot (issue #33) — subordinate to the
/// 20x20 class icon. Issue #187 bumped both up together (14 -> 16 here,
/// 18 -> 20 for `ICON_SIZE`) so the slot's ~0.8x-of-the-icon proportion —
/// smaller, secondary — is preserved rather than just growing one.
const IMAGINE_SIZE: f32 = 16.0;

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Gap between the class icon and Imagine slot 0, and between slot 0 and
/// slot 1. Not sourced from the reference meter — chosen so the gutter
/// arithmetic (`IMAGINE_GUTTER_WIDTH`) lands cleanly.
const IMAGINE_GAP: f32 = 2.0;

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Width the two Imagine slots add to `ICON_GUTTER_WIDTH`: two
/// `(gap + slot)` pairs, `32.0`. A single named addend so D4's takedown is
/// mechanical — deleting this line (and its use below) restores
/// `ICON_GUTTER_WIDTH` to its pre-issue-#33 `25.0` with no other
/// arithmetic to touch.
const IMAGINE_GUTTER_WIDTH: f32 = 2.0 * (IMAGINE_GAP + IMAGINE_SIZE);

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Dim fill for the blank-circle placeholder an empty, unknown-id, or
/// undecoded-texture Imagine slot paints instead of an icon — in the same
/// register as `CLASS_ICON_TINT`.
const IMAGINE_SLOT_EMPTY: egui::Color32 = egui::Color32::from_rgb(0x55, 0x55, 0x55);

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
const IMAGINE_MAX_TIER: i32 = 5;

/// Stroke color of the gold/amber ring `draw_row` paints around a
/// filled Imagine slot at `IMAGINE_MAX_TIER` (issue #170). Issue #180:
/// shifted off the original `#FFD700` ("gold" in name only — it rendered
/// as flat yellow) to `#D4AF37`, a warmer amber/gold that sits in the
/// `#D4AF37`-`#C9A227` range and still reads distinct from
/// `CLASS_ICON_TINT`'s neutral light gray as a deliberate highlight, not a
/// tint variation.
const IMAGINE_MAX_TIER_RING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xD4, 0xAF, 0x37);

/// Width of the gold max-tier ring's stroke (issue #170). Issue #180:
/// thinned from `1.5` to `1.0` so the ring reads as a thin accent rather
/// than dominating the 16pt `IMAGINE_SIZE` slot it circles.
const IMAGINE_MAX_TIER_RING_WIDTH: f32 = 1.0;

/// Hover-tooltip text for an equipped Imagine slot (issue #169): the plain
/// `name` when `tier` is absent or the wire-default `0` (proto3's
/// omit-when-default means "no tier observed yet" and "tier is genuinely
/// zero" are indistinguishable on the wire, so both read as "nothing to
/// add"), otherwise `"{name} · Tier {tier}"`.
fn imagine_hover_text(name: &str, tier: Option<i32>) -> String {
    match tier {
        Some(t) if t > 0 => format!("{name} · Tier {t}"),
        _ => name.to_string(),
    }
}

/// Whether a filled Imagine slot should get the gold max-tier ring (issue
/// #170): `tier >= IMAGINE_MAX_TIER`. `None` (unresolved/no tier data) and
/// any tier below the max both yield `false` — see `IMAGINE_MAX_TIER`'s doc
/// comment for why this is `>=` rather than `==`.
fn imagine_ring_visible(tier: Option<i32>) -> bool {
    tier.is_some_and(|t| t >= IMAGINE_MAX_TIER)
}

/// Fixed left-hand gutter `draw_row` reserves for the class icon plus its
/// two Imagine slots (issue #33): a margin, the class icon, the Imagine
/// gutter, then a matching margin — reserved whether or not this
/// particular row has any of these to paint, so every row's name still
/// starts at the same x (see `icon_slots`).
const ICON_GUTTER_WIDTH: f32 = ICON_MARGIN + ICON_SIZE + IMAGINE_GUTTER_WIDTH + ICON_MARGIN;

/// A row's class-icon slot, its two Imagine slots (issue #33), and the
/// x-offset from the row rect's left edge at which the player name should
/// then start.
#[derive(Clone, Copy, PartialEq, Debug)]
struct RowIconSlots {
    class: egui::Rect,
    imagines: [egui::Rect; 2],
    name_offset: f32,
}

/// Computes a row's class icon slot (a square, vertically centered in
/// `rect`, inset from the left edge by `ICON_MARGIN`), its two Imagine
/// slots immediately to its right (issue #33), and the x-offset from
/// `rect`'s left edge at which the player name should then start. Pure
/// geometry — it never looks at whether this row actually has a class icon
/// to paint or any equipped Imagines — so the slots, and therefore the
/// name's start position, are identical across every row regardless of
/// which classes have icons or which Imagines are equipped.
fn icon_slots(rect: egui::Rect) -> RowIconSlots {
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
fn row_name(row: &PlayerRow) -> String {
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
fn name_suffix(row: &PlayerRow, settings: &Settings) -> Option<String> {
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

/// Compact damage/DPS abbreviation (issue #118): `999`, `1.23K`, `12.34K`,
/// `123.4K`, `1.23M`, `1000K`. Below 1000 raw the value is left as a plain
/// integer — no suffix, no decimals. At or above 1000 it is scaled by
/// K/M/B as usual, and the *scaled* value's own magnitude then picks the
/// decimal count so the digit run stays ~4 significant figures without
/// ever growing wider than the Dps/Damage columns budget for: 2 decimals
/// below 100 (`1.23K`), 1 decimal below 1000 (`123.4K`), 0 decimals at or
/// above 1000 (`1000K`).
///
/// The 2- and 1-decimal bands are *truncated*, not rounded, to that
/// precision: e.g. a raw scaled value of `123.456` prints `"123.4"`, not
/// the `"123.5"` a naive `{:.1}` would round it to. Rust's `{:.N}` format
/// specifier rounds, so it is deliberately not used for those two bands —
/// truncating instead is what keeps a value's digit run stable as it
/// crosses a decimal-of-precision's worth of noise, and matches this
/// function's own reference table exactly (issue #118's `12345 ->
/// "12.34K"`, not `"12.35K"`).
///
/// The coarsest, 0-decimal band is the one exception: reaching it still
/// rounds, because that's the only band whose *threshold itself* is
/// decided by rounding. A scaled value in `[999.5, 1000)` (e.g. `999.95`,
/// issue #118's `999_950 -> "1000K"`) is close enough to the next full
/// order of magnitude that showing its truncated 1-decimal form
/// (`"999.9K"`) would read as more precise than it is — rounding the whole
/// value to the nearest integer first, and taking the 0-decimal band
/// whenever *that* reaches `1000`, is what escalates it to `"1000K"`
/// instead. Every arithmetic step here is exact integer division (`u128`,
/// to leave headroom for `av * 100` without overflow) rather than `f64`:
/// floating-point division of an exact decimal like `12345.0 / 1000.0`
/// does not always land on the exact decimal `12.345`, so an
/// integer-vs-integer comparison replaces what would otherwise be a
/// binary-representation-dependent truncation.
pub fn fmt_short(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let av = v.unsigned_abs();

    fn scaled(sign: &str, av: u64, divisor: u64, suffix: &str) -> String {
        let av = av as u128;
        let divisor = divisor as u128;

        // Round `av / divisor` to the nearest whole number (round-half-up;
        // `av` and `divisor` are both non-negative, and every divisor here
        // is even, so `divisor / 2` is exact) to decide whether the value
        // has effectively reached the next full order of magnitude.
        let rounded_ones = (av + divisor / 2) / divisor;
        if rounded_ones >= 1000 {
            return format!("{sign}{rounded_ones}{suffix}");
        }

        if av >= 100 * divisor {
            let tenths = (av * 10) / divisor;
            return format!("{sign}{}.{}{suffix}", tenths / 10, tenths % 10);
        }

        let hundredths = (av * 100) / divisor;
        format!("{sign}{}.{:02}{suffix}", hundredths / 100, hundredths % 100)
    }

    if av >= 1_000_000_000 {
        scaled(sign, av, 1_000_000_000, "B")
    } else if av >= 1_000_000 {
        scaled(sign, av, 1_000_000, "M")
    } else if av >= 1_000 {
        scaled(sign, av, 1_000, "K")
    } else {
        format!("{sign}{av}")
    }
}

/// Fight duration as `mm:ss` — zero-padded to two digits below ten minutes
/// (issue #91: the reference render shows `02:39`, where we showed `2:39`).
///
/// There is no hours field and deliberately still isn't: minutes are always
/// the leading field, so they are always padded and simply keep counting up
/// past 59 (`60:00`, `120:00`) rather than rolling over. A raid pull that
/// ran an hour reads more directly as `75:12` than as `1:15:12`, and the
/// stat row's width budget
/// (`the_stat_pills_fit_the_default_window_width`) is measured against the
/// `120:00` worst case. `{:02}` only ever pads, never truncates, so those
/// long durations are unaffected.
pub fn fmt_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins:02}:{secs:02}")
}

/// Damage-share percentage as `12.3%`.
pub fn fmt_share(share_pct: f32) -> String {
    format!("{share_pct:.1}%")
}

/// Crit/Lucky percentage as a whole number, `73%` (issue #80.2) — unlike
/// `fmt_share`'s one decimal, the reference render
/// (`docs/reference/new-shinra-ex.webp`) shows these two with none.
pub fn fmt_pct0(pct: f32) -> String {
    format!("{pct:.0}%")
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

/// Gap `draw_row` leaves between the icon gutter (issue #9's
/// `ICON_GUTTER_WIDTH`, widened by issue #33's Imagine slots) and where the
/// player name starts (`icon_slots`'s `name_offset`). Predates the icon
/// slot — this used to be measured from the row's own left edge — but
/// keeps its name since it's still the same "breathing room before the
/// name" budget.
const NAME_LEFT_PAD: f32 = 2.0;

/// Alpha `draw_row` paints the `AbilityScore`/`SeasonStrength` name-suffix
/// text at (issue #168) — a dimmed variant of the name's own opaque white,
/// so the bracketed score reads as secondary metadata trailing the name
/// rather than as part of the name itself. `0x99` (~60%) is dim enough to
/// read as de-emphasized against the name's full white while staying
/// comfortably legible against the row background.
const NAME_SUFFIX_ALPHA: u8 = 0x99;

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

/// Issue #84: the floor `draw_rows` holds `column_anchors`' shrink-to-fit
/// `scale` factor at once a narrow viewport would otherwise push it lower.
/// Below this floor, `draw_rows` stops shrinking columns and switches to
/// horizontal scrolling for the remaining overflow instead — see
/// `draw_rows`' `content_width`. `0.6` keeps every column's text legible
/// (a DPS column at `Dps` width 48 still budgets ~29pt, comfortably wider
/// than `FONT_SIZE_ROW`'s tallest digit) while still absorbing a fair
/// amount of narrowing before scrolling kicks in.
const MIN_COLUMN_SCALE: f32 = 0.6;

/// Default opening height (issue #26; extended by issue #9 slice 2's title
/// line): the whole header band + separator + a full 20-row raid roster, so
/// no scrolling is needed on first launch.
///
/// The header term is `header_band_height` itself, not a re-sum of the rows
/// inside it, so the two can never drift. That matters as of issue #91: the
/// band now reserves the subtitle's line and gap unconditionally, where this
/// used to leave them out on the grounds that the subtitle was conditional
/// and the default window could assume it absent. It no longer can — that
/// assumption was what left the window 18pt short of the 20 rows it
/// promises.
///
/// Decision 3: `draw_rows` zeroes `item_spacing.y` for its own scope, so
/// rows are truly contiguous (`ROW_HEIGHT` is the full 30pt pitch, no
/// separate gap) and there is no gap between the separator and the first
/// row either. Of the gaps above the row list, both that survive are
/// already inside the band except the stat row->separator one, which pays
/// the layout's ordinary `ITEM_SPACING_Y`.
///
///   band (68.0) + separator (6.0) + 20 rows * 30.0 (600.0) + gap (2.0)
///     = 676.0
///
/// The band/gap/separator terms are `first_player_row_top_offset` — the
/// same offset the header wash's height derives from (issue #158) — so
/// this window's "no scrolling needed" promise and the wash's reach can
/// never drift back out of sync with each other.
fn default_inner_height() -> f32 {
    inner_height_for_rows(DEFAULT_VISIBLE_ROWS)
}

/// Number of player rows the header dropdown's "Reset to defaults" item
/// (issue #203) resizes the window to fit — a small sample of the roster,
/// deliberately much shorter than `DEFAULT_VISIBLE_ROWS`' full 20-player
/// raid the tray's own `TrayCommand::ResetWindow` targets. That tray reset
/// is a different, OS-level "back to launch size" action and is untouched
/// by this one.
const RESET_TO_DEFAULTS_VISIBLE_ROWS: usize = 5;

/// Window height the header dropdown's "Reset to defaults" item resizes to
/// (issue #203): the same header-band/separator/gap math as
/// `default_inner_height`, just sized for `RESET_TO_DEFAULTS_VISIBLE_ROWS`
/// rows instead of the launch default's full `DEFAULT_VISIBLE_ROWS` raid.
/// Width is unaffected — `default_inner_width` already sizes for exactly
/// the `Settings::default()` column set this reset also restores, with no
/// dependence on row count — so only height needs its own helper here.
fn reset_to_defaults_inner_height() -> f32 {
    inner_height_for_rows(RESET_TO_DEFAULTS_VISIBLE_ROWS)
}

/// Shared formula behind both `default_inner_height` and
/// `reset_to_defaults_inner_height` (issue #203 review finding): the header
/// band + separator + gap above the roster, plus `rows` player rows below
/// it. Pulling this out means the two callers can never drift from each
/// other by editing the top-level math in one and not the other — only the
/// row count differs between them, and that lives in their own constants.
fn inner_height_for_rows(rows: usize) -> f32 {
    let rows = rows as f32 * ROW_HEIGHT;
    first_player_row_top_offset(header_band_height(BUTTON_ROW_HEIGHT)) + rows
}

/// Extra width folded into `default_inner_width` on top of the row-column
/// budget below, so the window opens wide enough to lay out the header's
/// stat row without wrapping. That row (issue #59's real rasterized pill
/// glyphs, the timer readout, and issue #62's 58pt inert status-toggle
/// cluster) is measured independently by `the_stat_pills_fit_the_default_
/// window_width`, which is the ground truth this headroom exists to
/// satisfy; 20pt clears the gap with room to spare across minor
/// font-metric variance between environments.
const HEADER_ROW_EXTRA_WIDTH: f32 = 20.0;

/// Default opening width (issue #26, widened for issue #9's icon gutter and
/// again for issue #33's two Imagine slots): a name budget in front of the
/// default stat columns' combined fixed width (read out of
/// `ColumnKind::spec` for whatever `Settings::default` enables, never
/// hardcoded), so names don't visually collide with them — plus the fixed
/// icon gutter now reserved at the row's left edge, so adding it doesn't
/// squeeze the name budget or the stat columns relative to before issue #9
/// — plus `HEADER_ROW_EXTRA_WIDTH` so the header's own (now wider) stat row
/// fits too.
///
///   icon gutter (class 3.5 + 18.0 + Imagines 32.0 + 3.5 = 57.0) + left pad (2.0)
///     + name budget (150.0) + gap (10.0)
///     + columns (DPS 80.0 + crit 56.0 + lucky 56.0 + deaths 48.0 = 240.0)
///     + right margin (4.0) + header row headroom (20.0) = 483.0
///
/// The columns term grew with issue #49's death column joining the default
/// set; because it is summed rather than written down, the default window
/// widened to keep the same name budget instead of quietly squeezing it.
/// Issue #33's `IMAGINE_GUTTER_WIDTH` addend widens this the same way: the
/// name budget stays `150.0` rather than being squeezed to make room.
fn default_inner_width() -> f32 {
    let columns_width: f32 = stat_columns_for(&Settings::default().stat_columns())
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

/// Sane upper bound, in points, for a persisted `window_size` axis —
/// comfortably past any real display (an 8K panel spans roughly 7680
/// logical px even before DPI scaling, and a multi-monitor span widens that
/// further, but nowhere near this far) so a legitimate ultra-wide setup
/// still restores fine while a corrupted settings.json (a bit-flipped or
/// hand-edited float) can't ask wgpu to allocate a multi-million-point
/// swapchain.
const MAX_INNER_SIZE_DIMENSION: f32 = 20_000.0;

/// Sane upper bound, in points², for a persisted `window_size`'s total
/// area. `MAX_INNER_SIZE_DIMENSION` alone only bounds each axis
/// independently, so a value like `[19_999.0, 19_999.0]` — comfortably
/// under the per-axis cap on both axes — would still ask wgpu for a
/// ~400-million-point swapchain. 64,000,000 comfortably covers an 8K panel
/// (7680x4320 ≈ 33.2 million) plus room for a wide multi-monitor span,
/// while still rejecting a corrupted settings.json that maxes out both
/// axes at once. Computed in f64 so the multiplication itself can never
/// overflow, whatever the axis values are.
const MAX_INNER_SIZE_AREA: f64 = 64_000_000.0;

/// Clamps a persisted `window_size` to something guaranteed openable, or
/// rejects it outright back to `None` (today's default size) if it's beyond
/// saving. Each axis is floored at `MIN_INNER_SIZE` — the same floor
/// `with_min_inner_size` enforces below, so a restored size is never asked
/// to start smaller than winit would allow anyway — and the whole value is
/// rejected when either axis is non-finite, either axis is larger than
/// `MAX_INNER_SIZE_DIMENSION`, or the total area is larger than
/// `MAX_INNER_SIZE_AREA`. A hand-edited or otherwise corrupted
/// settings.json must never be able to open an unusable overlay.
fn sanitize_window_size(size: [f32; 2]) -> Option<[f32; 2]> {
    let [w, h] = size;
    if !w.is_finite()
        || !h.is_finite()
        || w > MAX_INNER_SIZE_DIMENSION
        || h > MAX_INNER_SIZE_DIMENSION
        || (w as f64) * (h as f64) > MAX_INNER_SIZE_AREA
    {
        return None;
    }
    Some([w.max(MIN_INNER_SIZE.x), h.max(MIN_INNER_SIZE.y)])
}

/// Overlay window shape: always-on-top, borderless, transparent, sized to
/// fit a full raid by default (issue #26). `window_position` is the
/// last-saved position (issue #27, `Settings::window_position`) to reopen
/// at, or `None` on a first launch / wiped settings file, which leaves the
/// position to today's default OS/winit placement. `window_size` is the
/// last-saved inner size (issue #134, `Settings::window_size`), sanity-
/// clamped by `sanitize_window_size` — a garbage or absent value falls back
/// to `default_inner_width`/`default_inner_height`, never to something
/// unusable.
pub fn viewport(
    window_position: Option<[f32; 2]>,
    window_size: Option<[f32; 2]>,
) -> egui::ViewportBuilder {
    let inner_size = window_size
        .and_then(sanitize_window_size)
        .unwrap_or([default_inner_width(), default_inner_height()]);
    let mut builder = egui::ViewportBuilder::default()
        .with_always_on_top()
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_inner_size(inner_size)
        .with_min_inner_size(MIN_INNER_SIZE);
    if let Some(position) = window_position {
        builder = builder.with_position(position);
    }
    builder
}

/// Panel fill: near-black, the overlay's own value rather than the source's
/// `WindowData.DefaultBackgroundColor` `#232830` @ 0.5. That slate grey reads
/// as washed-out over game footage; the original ShinraMeter silhouette is
/// near-black, so we keep `#121216` here. Fixed constants deliberately — the
/// source binds all three of these to a settings VM, and user-configurable
/// chrome is out of scope for now.
///
/// Fully opaque at rest (issue #182). This used to carry a baked-in 200/255,
/// which meant `settings.opacity` multiplied *two* transparencies together
/// and the slider's 100% end painted a ~78%-opaque panel — the endpoints did
/// not mean what they said. Transparency now comes from exactly one place,
/// `settings.opacity` at the `Frame` call site, so 0% is gone and 100% is
/// solid. The pre-#182 look is still reachable, one slider drag away at ~78%.
/// The skill window's fills follow the same rule (issue #184), which is what
/// makes the two windows track each other at a given slider value.
const PANEL_FILL: egui::Color32 = egui::Color32::from_rgb(18, 18, 22);
/// Panel border: `DefaultBorderColor` `#717b85`, at the same 0.5 opacity the
/// source applies to the whole Border (fill and stroke alike). Unlike
/// `PANEL_FILL` above, this alpha deliberately survives issue #182: it is a
/// *color* choice — it is what makes a 1px edge read as a hairline against
/// the fill rather than a bright grey outline — not a second transparency
/// knob competing with the slider. It still fades to nothing at 0%.
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
const SKILL_CHROME_FILL: egui::Color32 = egui::Color32::from_rgb(0x11, 0x11, 0x17);
/// Panel/tab background, and the Deaths pill's fill — the reference's
/// `#212127`. Same opaque-baseline rule as `SKILL_CHROME_FILL`.
const SKILL_PANEL_FILL: egui::Color32 = egui::Color32::from_rgb(0x21, 0x21, 0x27);
/// Dps column-header text — the reference's `#ef5350`.
const SKILL_HEADER_RGB: egui::Color32 = egui::Color32::from_rgb(0xef, 0x53, 0x50);
/// Close glyph — the reference's `LightRed #ff5555`.
const SKILL_CLOSE_RGB: egui::Color32 = egui::Color32::from_rgb(0xff, 0x55, 0x55);
/// Translucent-white row hover — the reference's `#10FFFFFF`. Its alpha is
/// the highlight's own weight; issue #184 multiplies `settings.opacity` in on
/// top of it at the paint site, because this fill is part of the window's
/// chrome layer (it paints straight onto `SKILL_CHROME_FILL`, under the row's
/// text) and would otherwise be left hovering over nothing once the rest of
/// the window faded. The main row list's `row_hover_quads` gradient is
/// knowingly *not* treated this way: it belongs to the row-content layer that
/// #166 keeps at full alpha.
const SKILL_ROW_HOVER_FILL: egui::Color32 =
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
const SKILL_COLUMN_HEADER_FILL: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x2a, 0x30);

/// Band heights, all measured off `docs/reference/shinra-skills-ex.webp`
/// (issue #200). That capture is 1:1 with WPF DIPs — its header pill is
/// exactly 34px tall against `Skills.xaml`'s `CornerRadius="17"`, which
/// pins the scale — so the pixel figures port straight to egui points.
/// Header: content top y=2 to y=70. Tab strip: y=71..92. Column header:
/// y=93..132. Rows: a 44px pitch from y=133 (ten row text centers, 156
/// through 553, evenly spaced 44.1px apart).
const SKILL_HEADER_HEIGHT: f32 = 70.0;
const SKILL_TAB_HEIGHT: f32 = 22.0;
const SKILL_COLUMN_HEADER_HEIGHT: f32 = 40.0;
/// The reference's measured 44px row pitch (issue #200) — taller than the
/// main row list's `ROW_HEIGHT` (30.0), so this is its own constant rather
/// than reusing that one. D5/D14 read `Skills.xaml`'s `MinHeight` of 40 as
/// the row height; the rendered capture shows 44 once the row's own padding
/// is included, and the pitch is what the eye actually reads.
const SKILL_ROW_HEIGHT: f32 = 44.0;
/// The per-skill row icon (issue #192), measured off the reference (issue
/// #200): row 1's disc spans x 32..69 and y 136..173, i.e. 38px across in a
/// 44px row — the icon dominates its row rather than sitting as a small
/// bullet beside the name, which 24.0 made it. The vendored PNGs are 48px
/// (`scripts/prep-skill-icons.py`), so this is still a downscale at 100%
/// display scaling.
const SKILL_ICON_SIZE: f32 = 38.0;
/// Fill for a row whose skill has no icon to paint. Deliberately the same
/// flat disc the Imagine slots degrade to, so an empty slot reads as a
/// deliberate blank rather than a rendering failure.
const SKILL_ICON_EMPTY: egui::Color32 = egui::Color32::from_rgb(0x33, 0x33, 0x3B);
/// `Skills.xaml:151-154` draws the class icon at 50x50. Issue #190 could
/// only fit 40 of that, because `SKILL_HEADER_HEIGHT` was a made-up 56 and
/// 50 would have overflowed its padded content area. Issue #200 measured
/// the reference's header band at 70px instead, so the source's 50 now
/// lands verbatim *and* keeps the "icon exactly fills the padded row"
/// relationship #190 was reaching for
/// (`SKILL_HEADER_HEIGHT - 2 * SKILL_HEADER_PAD_Y` = 50).
const SKILL_HEADER_ICON_SIZE: f32 = 50.0;
const SKILL_HEADER_PAD_X: f32 = 12.0;
const SKILL_HEADER_PAD_Y: f32 = 10.0;
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
const SKILL_CLOSE_HIT_SIZE: f32 = 32.0;
/// The side of the cross's own box inside that target — the reference's
/// `Path … Width="16"`. Painted as two strokes rather than set as text:
/// `U+2715` is not covered by `fonts::bold_family`'s chain and came out as
/// tofu (an empty box), which is what issue #218 called a "square" close
/// button, and the reference's `Svg.Close` is vector art anyway.
const SKILL_CLOSE_GLYPH_SIZE: f32 = 16.0;
/// Stroke weight of those two strokes. `Svg.Close` is a filled path with no
/// nominal weight; 1.6pt is what reads as the same visual density at 16pt
/// against `SKILL_CLOSE_RGB`.
const SKILL_CLOSE_STROKE_WIDTH: f32 = 1.6;
/// The scroll thumb's fill: white at ~20% over the panel, the same read as
/// the reference's thin light thumb. Faded with the rest of the chrome
/// (issue #184).
const SKILL_SCROLL_THUMB_FILL: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
/// The thumb never gets shorter than this, however long the list — a
/// two-pixel nub is not a grabbable or readable position indicator.
const SKILL_SCROLL_THUMB_MIN_HEIGHT: f32 = 24.0;
/// Width of the row list's scrollbar, thumb and track alike (issue #218) —
/// the reference's persistent thin thumb. Also the gutter
/// `skill_rows_content_rect` reserves for it.
const SKILL_SCROLL_BAR_WIDTH: f32 = 6.0;
/// The hover wash `ButtonMainStyle`'s `hl` border flips to on `IsMouseOver`:
/// WPF's 4-digit ARGB `#1fff` — white at alpha `0x11`. Spelled premultiplied
/// because `Color32::from_white_alpha`, which is exactly this, is not `const`.
const SKILL_CLOSE_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0x11, 0x11, 0x11, 0x11);
/// The reference's `CornerRadius="17"` header pill.
const SKILL_PILL_CORNER_RADIUS: u8 = 17;
/// Header pill height, measured at 34px in the reference (issue #200) —
/// exactly `2 * SKILL_PILL_CORNER_RADIUS`, i.e. a true stadium. It used to
/// be derived from the header band instead, which made it 40 tall against a
/// 17 radius: a rounded rectangle with flat sides, not the reference's pill.
const SKILL_PILL_HEIGHT: f32 = 34.0;
/// The reference's 24pt player name — the one size in this window with no
/// equivalent in the main row scale (`FONT_SIZE_ROW` tops out at 13.0).
const FONT_SIZE_SKILL_HEADER_NAME: f32 = 24.0;

/// D5's column order, as a fixed array so the header row and every data
/// row iterate it identically — a column can never appear in one but not
/// the other.
const SKILL_COLUMN_ORDER: [skills::SkillColumn; 12] = [
    skills::SkillColumn::Icon,
    skills::SkillColumn::Name,
    skills::SkillColumn::Damage,
    skills::SkillColumn::DmgPct,
    skills::SkillColumn::CritPct,
    skills::SkillColumn::MaxCrit,
    skills::SkillColumn::AvgCrit,
    skills::SkillColumn::AvgWhite,
    skills::SkillColumn::Avg,
    skills::SkillColumn::Hits,
    skills::SkillColumn::Crits,
    skills::SkillColumn::HitPerMin,
];

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
const SKILL_WINDOW_SIZE: egui::Vec2 = egui::vec2(760.0, 572.0);
/// Floor on the skill breakdown viewport's inner size (issue #181) so a
/// resize can't shrink it into uselessness — tall enough for the header, tab
/// strip and column-header row plus a couple of rows before the list
/// scrolls, wide enough to keep the columns legible once
/// `column_anchors_from_widths` scales them down.
const SKILL_WINDOW_MIN_SIZE: egui::Vec2 = egui::vec2(360.0, 220.0);

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
struct SkillWindowState {
    sort: skills::SkillSort,
    pos: egui::Pos2,
    size: egui::Vec2,
    source: SkillWindowSource,
    /// Issue #218: this window's own in-flight move/resize. Per-window
    /// rather than shared with the root's, because two viewports can be
    /// dragged in two different (non-overlapping) sessions and because
    /// `drive_window_gesture` sends its viewport commands to whichever
    /// context is live — inside `show_viewport_immediate`'s callback that
    /// is this child, not the root.
    gesture: WindowGesture,
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
enum SkillWindowSource {
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
fn open_skill_window(
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
fn skill_viewport_id(uid: i64) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("skills", uid))
}

/// The only two paths that may drop a uid from the open-window set (D2):
/// the in-window `X` and an OS-level close request. Never called for a uid
/// merely missing from the current snapshot — see `skill_windows_to_draw`.
fn close_skill_window(windows: &mut std::collections::BTreeMap<i64, SkillWindowState>, uid: i64) {
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
fn track_skill_window_size(size: &mut egui::Vec2, inner_rect: Option<egui::Rect>) {
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
const SKILL_WINDOW_SIZE_EPSILON: f32 = 1.0;

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
fn skill_window_rows<'a>(
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
fn skill_windows_to_draw<'a>(
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
fn skill_selected_tab_rect(tabs_rect: egui::Rect, text_width: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        tabs_rect.min,
        egui::vec2(text_width + 2.0 * SKILL_HEADER_PAD_X, tabs_rect.height()),
    )
}

/// The column-header band's rect (issue #200): flush with the window's
/// left/right edges, directly beneath the tab strip, `SKILL_COLUMN_HEADER_
/// HEIGHT` tall. Painted with `SKILL_COLUMN_HEADER_FILL`.
fn skill_column_header_rect(rect: egui::Rect, tabs_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(rect.left(), tabs_rect.bottom()),
        egui::vec2(rect.width(), SKILL_COLUMN_HEADER_HEIGHT),
    )
}

/// The scrollable row-list band's rect (issue #200): everything below the
/// column header down to the window's bottom edge. Painted with
/// `SKILL_PANEL_FILL`, not the window's `SKILL_CHROME_FILL`.
fn skill_rows_rect(rect: egui::Rect, col_header_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(rect.left(), col_header_rect.bottom()), rect.max)
}

/// The message painted where the row list would go when a breakdown window
/// has nothing to show (issue #216) — which of the two it is depends on the
/// window's source, not just on the row count (PR #221 review).
///
/// A historical fight's `PlayerRow::skills` is empty *permanently*: the
/// history schema doesn't persist per-skill totals (see
/// `history::PlayerRecord::to_row`), so nothing will ever populate it and
/// naming that limitation is what keeps the window from reading as silent
/// breakage. A live row's empty `skills` means only "not yet": the dungeon
/// roster preload (`encounter::apply_player`) puts a party member in the
/// snapshot with an empty skill map before their first hit lands, and a
/// healer can sit there for a whole fight — telling that user "nothing was
/// recorded for this fight" would be plainly wrong while the fight is still
/// running, so the live wording promises the rows are coming.
fn skill_window_empty_message(
    source: SkillWindowSource,
    skill_row_count: usize,
) -> Option<&'static str> {
    if skill_row_count > 0 {
        return None;
    }
    Some(match source {
        SkillWindowSource::Live => "No damage recorded yet",
        SkillWindowSource::History(_) => "No per-skill data recorded for this fight",
    })
}

/// The header band: full width, `SKILL_HEADER_HEIGHT` tall, flush with the
/// window's top. Pulled out of `draw_skill_window` (issue #218) so the drag
/// band and close button derived from it are testable without a live `Ui`.
fn skill_header_rect(rect: egui::Rect) -> egui::Rect {
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
fn skill_drag_band(header_rect: egui::Rect) -> egui::Rect {
    let mut band = header_rect;
    band.min.y += RESIZE_EDGE;
    band.min.x += RESIZE_EDGE;
    band.max.x -= RESIZE_EDGE;
    band
}

/// The close button's hit square (issue #218), top-right of the window
/// inside the header's padding. `SKILL_CLOSE_HIT_SIZE` wide, so it is also
/// the bounding box of the circular hover wash painted at its centre.
fn skill_close_rect(rect: egui::Rect) -> egui::Rect {
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
fn skill_close_cross(close_rect: egui::Rect) -> [[egui::Pos2; 2]; 2] {
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
fn skill_rows_content_rect(rows_rect: egui::Rect) -> egui::Rect {
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
fn skill_scroll_thumb(
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

/// Where the Deaths pill's left edge lands: right-aligned into the header,
/// one `SKILL_HEADER_PAD_X` clear of the close button. Shares
/// `SKILL_CLOSE_HIT_SIZE` with `skill_close_rect` so growing that button
/// (issue #218) pushes the pill left instead of letting the two overlap.
fn skill_deaths_pill_left(header_rect: egui::Rect, pill_width: f32) -> f32 {
    header_rect.right()
        - SKILL_HEADER_PAD_X
        - SKILL_CLOSE_HIT_SIZE
        - SKILL_HEADER_PAD_X
        - pill_width
}

fn draw_skill_window(
    ui: &mut egui::Ui,
    row: &PlayerRow,
    sort: &mut skills::SkillSort,
    source: SkillWindowSource,
    icons: &Icons,
    opacity: f32,
    gesture: &mut WindowGesture,
) -> bool {
    let rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, SKILL_CHROME_FILL.gamma_multiply(opacity));

    // Issue #218: this window is `with_decorations(false)` like the root, so
    // winit cancels `WS_SIZEBOX` and hands back no OS resize frame — the
    // `with_resizable(true)` on its builder is dead. It supplies its own
    // grips exactly as the root window does. Registered first so the header
    // widgets below win the pixels they overlap; egui gives interaction
    // priority to whatever was registered later.
    draw_resize_handles(ui, &ctx, gesture, ("skill", row.uid));

    // -- header: class icon, player name, one Deaths pill (D10) ----------
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
    paint_text(
        &painter,
        egui::pos2(
            icon_rect.right() + SKILL_HEADER_PAD_X,
            header_rect.center().y,
        ),
        egui::Align2::LEFT_CENTER,
        &row.name,
        regular(FONT_SIZE_SKILL_HEADER_NAME),
        egui::Color32::WHITE,
        false,
    );

    // D10: only the Deaths pill. The reference's other three header pills
    // (death time, aggro count, aggro time) need revive-timing and threat/
    // aggro data this decoder never captures — rendering them would be
    // inventing numbers rather than reporting them. Follow-up candidate,
    // noted in the PR body.
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
        fill: SKILL_PANEL_FILL.gamma_multiply(opacity),
        stroke: None,
    };
    let deaths_text_size = pill_text_size(&painter, &deaths_pill);
    let deaths_pill_size = pill_size(deaths_text_size, deaths_pill.icon_side, SKILL_PILL_HEIGHT);
    let deaths_pill_rect = egui::Rect::from_min_size(
        egui::pos2(
            skill_deaths_pill_left(header_rect, deaths_pill_size.x),
            header_rect.center().y - deaths_pill_size.y / 2.0,
        ),
        deaths_pill_size,
    );
    paint_stat_pill(&painter, deaths_pill_rect, deaths_text_size, &deaths_pill);

    // -- close glyph (D2): the only in-window way to close ---------------
    // Issue #218: interacted *before* it is painted, because the hover wash
    // is part of the paint — a 32pt circle behind the glyph, matching the
    // reference's `ButtonMainStyle` (`#1fff` on `IsMouseOver`, radius =
    // half the side) — and a pointing-hand cursor. The glyph itself used to
    // be the whole button: a 20pt square with no radius, no hover feedback
    // and no cursor change, so nothing about it read as clickable.
    let close_rect = skill_close_rect(rect);
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

    // -- tab strip: `Dps` only, styled selected (D11) ---------------------
    // The reference's other six tabs (Heal, Mana, Buff, Counter,
    // SkillDealt, SkillReceived) are explicitly out of scope per the issue
    // — drawing six dead tabs would be clutter, not fidelity.
    let tabs_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), header_rect.bottom()),
        egui::vec2(rect.width(), SKILL_TAB_HEIGHT),
    );
    // Issue #200: the strip itself is *not* a filled band. At y=80 in the
    // reference the pixels under the unselected tabs (x=700) match the
    // header band exactly, while only the selected `Dps` tab (x 2..51)
    // carries the lighter `#212127` box. Filling the whole strip made the
    // window read as a two-tone sandwich instead of a tab row.
    let tab_label = "Dps";
    let tab_font = bold(FONT_SIZE_ROW);
    let tab_text_width = painter
        .layout_no_wrap(tab_label.to_owned(), tab_font.clone(), egui::Color32::WHITE)
        .size()
        .x;
    painter.rect_filled(
        skill_selected_tab_rect(tabs_rect, tab_text_width),
        0.0,
        SKILL_PANEL_FILL.gamma_multiply(opacity),
    );
    paint_text(
        &painter,
        tabs_rect.left_center() + egui::vec2(SKILL_HEADER_PAD_X, 0.0),
        egui::Align2::LEFT_CENTER,
        tab_label,
        tab_font,
        egui::Color32::WHITE,
        true,
    );

    // -- column header row: click (either button, D9) toggles sort -------
    let col_header_rect = skill_column_header_rect(rect, tabs_rect);
    painter.rect_filled(
        col_header_rect,
        0.0,
        SKILL_COLUMN_HEADER_FILL.gamma_multiply(opacity),
    );
    let widths: Vec<f32> = SKILL_COLUMN_ORDER.iter().map(|c| c.width()).collect();
    let anchors = column_anchors_from_widths(
        col_header_rect.left() + SKILL_HEADER_PAD_X,
        col_header_rect.right() - SKILL_HEADER_PAD_X,
        &widths,
        0.0,
    );
    for (i, (&anchor_x, kind)) in anchors.iter().zip(SKILL_COLUMN_ORDER.iter()).enumerate() {
        let width = kind.width();
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
        let (align, pos) = if *kind == skills::SkillColumn::Name {
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
    painter.rect_filled(rows_rect, 0.0, SKILL_PANEL_FILL.gamma_multiply(opacity));
    let mut skill_rows = row.skills.clone();
    skills::sort_rows(&mut skill_rows, *sort);

    // Issue #216: an empty row list gets a message in place of the rows,
    // worded for where the window's data comes from — a historical fight
    // never has per-skill rows at all, a live one just doesn't have them
    // yet (see `skill_window_empty_message`).
    if let Some(message) = skill_window_empty_message(source, skill_rows.len()) {
        paint_text(
            &painter,
            rows_rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            regular(FONT_SIZE_ROW),
            egui::Color32::GRAY,
            false,
        );
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
                let (skill_rect, response) = ui.allocate_exact_size(
                    egui::vec2(rows_content_rect.width(), SKILL_ROW_HEIGHT),
                    egui::Sense::hover(),
                );
                if response.hovered() {
                    // Issue #184: fades with the rest of the chrome, same
                    // as the tab strip and the header pill.
                    ui.painter().rect_filled(
                        skill_rect,
                        0.0,
                        SKILL_ROW_HOVER_FILL.gamma_multiply(opacity),
                    );
                }
                for (&anchor_x, kind) in anchors.iter().zip(SKILL_COLUMN_ORDER.iter()) {
                    let width = kind.width();
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
                    // on the same blank-disc branch — one degrade path,
                    // never a panic, the same shape `ImagineIcons`' empty
                    // slot uses.
                    if *kind == skills::SkillColumn::Icon {
                        let center =
                            egui::pos2(clip.left() + SKILL_ICON_SIZE / 2.0, clip.center().y);
                        match skills::skill_icon_basename(skill.skill_id)
                            .and_then(|basename| icons.skills.get(basename))
                        {
                            Some(texture) => cell_painter.image(
                                texture.id(),
                                egui::Rect::from_center_size(
                                    center,
                                    egui::Vec2::splat(SKILL_ICON_SIZE),
                                ),
                                UV_FULL,
                                CLASS_ICON_TINT,
                            ),
                            None => cell_painter.circle_filled(
                                center,
                                SKILL_ICON_SIZE / 2.0,
                                SKILL_ICON_EMPTY,
                            ),
                        };
                        continue;
                    }
                    let (align, pos) = if *kind == skills::SkillColumn::Name {
                        (egui::Align2::LEFT_CENTER, clip.left_center())
                    } else {
                        (egui::Align2::RIGHT_CENTER, clip.right_center())
                    };
                    paint_text(
                        &cell_painter,
                        pos,
                        align,
                        &kind.text(skill),
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
            SKILL_SCROLL_THUMB_FILL.gamma_multiply(opacity),
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

// == Encounter history (issue #39) =======================================
//
// Everything below is WP3 of the persistent-history feature: the overlay's
// "past fights" surface. WP1 (`crate::history`) and WP2
// (`crate::history::writer`) already own storage and the background
// thread; this is the view over them.

/// Which surface the overlay is showing (issue #39). `History` is a *mode of
/// the existing `CentralPanel`*, not a second window: the main HWND is the
/// only one carrying `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition (issue
/// #89), the `WM_NCHITTEST` click-through carve-out (issue #167), the
/// Aero-Snap blocker (issue #11) and the tray subclass (issue #53) — a second
/// egui viewport would have none of them.
enum OverlayView {
    Live,
    // Boxed (issue #39, clippy::large_enum_variant): `HistoryUi` carries a
    // `Vec<EncounterSummary>` and an `Option<OpenEncounter>` — enough that
    // an unboxed variant would make every `OverlayView` (including the
    // common `Live` case) pay for the larger variant's size.
    History(Box<HistoryUi>),
}

/// The history view's own state: what the last reply from the history thread
/// contained, and which encounter (if any) is open.
#[derive(Default)]
struct HistoryUi {
    /// Newest-first summaries from the last `Listed` reply.
    encounters: Vec<history::EncounterSummary>,
    /// The encounter currently being read, already rebuilt into a `Snapshot`
    /// so `draw_rows` can render it unchanged — a past fight looks pixel
    /// identical to a live one.
    open: Option<OpenEncounter>,
    /// A `HistoryEvent::Failed` message worth showing, cleared on the next
    /// successful reply.
    error: Option<String>,
    /// True between firing a request and its reply landing — the view shows
    /// "Loading…" rather than a stale empty list.
    pending: bool,
    /// Latches "Clear all" into a confirming state after one click; a second
    /// click while this is true actually fires `HistoryHandle::clear`. Reset
    /// on every other history-bar/list interaction, so leaving and
    /// returning to the list never leaves it primed.
    confirm_clear: bool,
    /// The id of the newest in-flight `Load` request, if any. Rows stay
    /// clickable while one is in flight, so a `Loaded`/`Missing` reply
    /// carrying any other id belongs to a click the user has already
    /// superseded, and is dropped.
    pending_load_id: Option<i64>,
}

/// One saved encounter, rebuilt for display: the id (needed for the delete
/// button while it's open) plus everything `draw_header`/`draw_rows` need.
#[derive(Clone)]
struct OpenEncounter {
    // `id` is what a breakdown window opened from this fight stores as its
    // `SkillWindowSource::History` (issue #216), so a later frame can tell
    // "still the same fight" from "a different one is open now".
    // `ended_at_ms` rounds the DTO out to match `EncounterSummary`'s shape
    // (and is what a future "delete the fight I'm looking at" button would
    // need), but WP3's bar only offers delete from the list — so it is not
    // read yet.
    id: i64,
    title: String,
    subtitle: Option<String>,
    #[allow(dead_code)]
    ended_at_ms: u64,
    snapshot: Snapshot,
}

/// Whether the Share button may fire a screenshot capture this frame
/// (issue #219): true on `OverlayView::Live`, and on `OverlayView::History`
/// only once a specific historical encounter is open (`state.open` is
/// `Some`) — both render through the same `draw_rows` a DPS-style row list,
/// per the spec's reference-fidelity requirement. False on the bare history
/// list, where nothing behind the button is a row screenshot worth taking —
/// capturing it would just screenshot the list of past encounters. Pure so
/// the view -> active mapping is unit-testable without an `egui::Context`;
/// `OverlayApp::ui` is the only caller.
fn share_active_for_view(view: &OverlayView) -> bool {
    match view {
        OverlayView::Live => true,
        OverlayView::History(state) => state.open.is_some(),
    }
}

/// Which row count the Share crop bound (`rows_content_bottom_y`) must use
/// this frame (issue #219): the open historical encounter's own row count
/// while one is open, not the *live* snapshot's — the two can differ (a
/// past fight had a different roster, or the live encounter has since
/// ended down to zero rows) and cropping against the wrong one either cuts
/// off real rows or leaves stray blank space. `live_row_count` is the
/// fallback for `Live` and the (never actually captured, since Share is
/// inactive there — see `share_active_for_view`) bare history list. Pure so
/// this is unit-testable without an `egui::Context`.
fn screenshot_row_count(view: &OverlayView, live_row_count: usize) -> usize {
    match view {
        OverlayView::Live => live_row_count,
        OverlayView::History(state) => state
            .open
            .as_ref()
            .map(|open| open.snapshot.rows.len())
            .unwrap_or(live_row_count),
    }
}

/// `OverlayApp::ui`'s single entry point for both halves of the Share crop
/// bound (PR #225 review of issue #219): it must call `screenshot_row_count`
/// on `view` *before* applying the "← Live" reset (`back_to_live`), because
/// `draw_history` already painted `view`'s rows this exact frame — even on
/// the frame a "← Live" click sets `back_to_live`, since that click only
/// takes effect on the *next* frame's `match`. Reading the row count after
/// the reset would use the just-reset `OverlayView::Live` instead of the
/// historical view that was actually on screen, reintroducing the
/// crop-bound mismatch issue #219 fixed. Folding both steps into one
/// function — rather than the caller doing `screenshot_row_count` then a
/// separate `if back_to_live { … }` — is what keeps that ordering from
/// drifting apart under a future edit. Pure aside from the `&mut
/// OverlayView` reset, so the ordering itself is unit-testable without an
/// `egui::Context`.
fn resolve_screenshot_row_count(
    view: &mut OverlayView,
    back_to_live: bool,
    live_row_count: usize,
) -> usize {
    let row_count = screenshot_row_count(view, live_row_count);
    if back_to_live {
        *view = OverlayView::Live;
    }
    row_count
}

/// The header's title/subtitle override for a historical fight (spec
/// DECISION D7) — bundled rather than passed as two loose parameters so
/// `draw_header`'s argument list stays readable.
struct HistoryHeader<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
}

/// The ceiling on how many encounters the list requests, used when
/// `Settings::history_max_encounters` is `0` — "prune nothing by count",
/// which has no matching `LIMIT`. Equal to the settings cap, so the list can
/// never hide a row retention has kept.
const HISTORY_LIST_CEILING: u32 = Settings::HISTORY_MAX_ENCOUNTERS_CAP;

/// Width of the trailing delete button painted into each history row.
const HISTORY_DELETE_WIDTH: f32 = 18.0;

/// Left/right text inset inside a history row, matching the row list's own
/// breathing room rather than introducing a new metric scale.
const HISTORY_ROW_PADDING: f32 = 8.0;

/// The header's title/subtitle selection: a historical fight's saved name
/// (spec DECISION D7) when `history` is `Some`, the live encounter's derived
/// name otherwise. Pulled out of `draw_header` as the one pure extraction
/// WP3 permits, so it is testable without an `egui::Ui`.
fn header_text(
    snapshot: &Snapshot,
    history: Option<&HistoryHeader<'_>>,
) -> (String, Option<String>) {
    match history {
        Some(h) => (h.title.to_string(), h.subtitle.map(str::to_string)),
        None => (
            encounter_title(&snapshot.encounter),
            encounter_subtitle(&snapshot.encounter),
        ),
    }
}

impl OverlayApp {
    /// Drains the history thread's replies, once per frame (spec DECISION
    /// D5). Called from `ui()` unconditionally, before the panel — the
    /// channel is drained even in `Live` view (so replies never pile up
    /// behind `rx_history`), but a reply is only *applied* to `HistoryUi`
    /// while that view actually exists to hold it; `open_history` always
    /// issues a fresh `list` request on the way in, so a reply that arrives
    /// after the view has already closed is safe to simply discard.
    fn poll_history(&mut self) {
        let list_limit = self.history_list_limit();
        for event in self.rx_history.try_iter() {
            let OverlayView::History(state) = &mut self.view else {
                continue;
            };
            match event {
                history::writer::HistoryEvent::Listed(rows) => {
                    state.encounters = rows;
                    state.error = None;
                    state.pending = false;
                }
                history::writer::HistoryEvent::Loaded { id, record } => {
                    // A reply for anything but the newest click is stale:
                    // the rows stay clickable while a load is in flight, so
                    // the user may already have asked for a different fight.
                    if state.pending_load_id != Some(id) {
                        continue;
                    }
                    state.pending_load_id = None;
                    state.open = Some(OpenEncounter {
                        id,
                        title: record.title.clone(),
                        subtitle: record.subtitle.clone(),
                        ended_at_ms: record.ended_at_ms,
                        snapshot: record.to_snapshot(),
                    });
                    state.error = None;
                    state.pending = false;
                }
                history::writer::HistoryEvent::Missing(id) => {
                    // Deleted, or pruned since the list was taken — drop
                    // back to the list rather than showing a stale fight.
                    // Same staleness check as `Loaded`.
                    if state.pending_load_id != Some(id) {
                        continue;
                    }
                    state.pending_load_id = None;
                    state.open = None;
                    state.pending = false;
                }
                history::writer::HistoryEvent::Changed => {
                    // A delete/clear landed; re-request the list so it
                    // reflects the new state.
                    state.confirm_clear = false;
                    if let Some(handle) = &self.history {
                        handle.list(list_limit, &self.tx_history);
                        state.pending = true;
                    }
                }
                history::writer::HistoryEvent::Failed(message) => {
                    state.pending_load_id = None;
                    state.error = Some(message);
                    state.pending = false;
                }
            }
        }
    }

    /// How many encounters the list asks for: exactly what retention keeps,
    /// so an encounter that is on disk is always one the user can see and
    /// delete (issue #39).
    fn history_list_limit(&self) -> u32 {
        match self.settings.history_max_encounters {
            0 => HISTORY_LIST_CEILING,
            limit => limit.min(HISTORY_LIST_CEILING),
        }
    }

    /// Switches to the history view and asks for the list (issue #39).
    fn open_history(&mut self) {
        let pending = self.history.is_some();
        let list_limit = self.history_list_limit();
        self.view = OverlayView::History(Box::new(HistoryUi {
            pending,
            ..HistoryUi::default()
        }));
        if let Some(handle) = &self.history {
            handle.list(list_limit, &self.tx_history);
        }
    }
}

/// The whole history surface: the bar, then either the list or the open
/// encounter's rows (rendered through the same `draw_rows` a live fight
/// uses, per the spec's reference-fidelity requirement).
///
/// Returns the window-space y coordinate (points) where that content —
/// list or rows — actually starts, and the height left in the panel for it
/// at that point (issue #219): `OverlayApp::ui` used to capture both
/// *before* calling this function, from the outer panel's own separator,
/// but this function draws its own nav bar (`draw_history_bar`) and a
/// second separator first, so with a historical encounter open the real
/// rows start lower than that stale bound — the Share screenshot crop was
/// computed against too little content and cut the last row(s) off.
/// Measuring both right here, after this function's own chrome and before
/// its content, is what keeps the crop bound from drifting out of sync
/// with what actually gets painted underneath it.
#[allow(clippy::too_many_arguments)]
fn draw_history(
    ui: &mut egui::Ui,
    state: &mut HistoryUi,
    settings: &Settings,
    icons: &Icons,
    handle: Option<&history::writer::HistoryHandle>,
    tx: &Sender<history::writer::HistoryEvent>,
    back_to_live: &mut bool,
    // Issue #216: set to `Some(uid)` when a right-click on an open
    // historical fight's row is sensed this frame — the same out-parameter
    // `draw_rows` already threads for the live view (see its own doc
    // comment), plumbed one level further in so a historical row can open
    // its player's breakdown window too, instead of the click being
    // swallowed silently.
    opened: &mut Option<i64>,
) -> (f32, f32) {
    match draw_history_bar(ui, state) {
        HistoryBarAction::None => {}
        HistoryBarAction::Live => *back_to_live = true,
        HistoryBarAction::Back => {
            state.open = None;
            state.confirm_clear = false;
        }
        HistoryBarAction::ClearAll => {
            if state.confirm_clear {
                state.confirm_clear = false;
                if let Some(handle) = handle {
                    handle.clear(tx);
                    state.pending = true;
                }
            } else {
                state.confirm_clear = true;
            }
        }
    }

    ui.separator();

    let rows_top = ui.cursor().top();
    let rows_area_height = ui.available_height();

    if let Some(open) = &state.open {
        draw_rows(ui, &open.snapshot, settings, icons, opened);
        return (rows_top, rows_area_height);
    }

    match draw_history_list(ui, state) {
        Some(HistoryRowAction::Open(id)) => {
            state.confirm_clear = false;
            state.pending_load_id = Some(id);
            state.pending = true;
            if let Some(handle) = handle {
                handle.load(id, tx);
            }
        }
        Some(HistoryRowAction::Delete(id)) => {
            state.confirm_clear = false;
            state.pending = true;
            if let Some(handle) = handle {
                handle.delete(id, tx);
            }
        }
        None => {}
    }

    (rows_top, rows_area_height)
}

/// The bar under the header: "← Live", a "← Back" when an encounter is
/// open, and (in list mode) "Clear all".
fn draw_history_bar(ui: &mut egui::Ui, state: &HistoryUi) -> HistoryBarAction {
    let mut action = HistoryBarAction::None;
    ui.horizontal(|ui| {
        if ui.button("← Live").clicked() {
            action = HistoryBarAction::Live;
        }
        if state.open.is_some() {
            if ui.button("← Back").clicked() {
                action = HistoryBarAction::Back;
            }
        } else {
            let label = if state.confirm_clear {
                "Clear all — confirm?"
            } else {
                "Clear all"
            };
            if ui.button(label).clicked() {
                action = HistoryBarAction::ClearAll;
            }
        }
    });
    action
}

/// The newest-first list of saved encounters, one fixed-height row each.
/// Returns the row the user clicked, if any.
fn draw_history_list(ui: &mut egui::Ui, state: &HistoryUi) -> Option<HistoryRowAction> {
    if let Some(message) = &state.error {
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), message.as_str());
        return None;
    }
    if state.pending && state.encounters.is_empty() {
        ui.label("Loading…");
        return None;
    }
    if state.encounters.is_empty() {
        ui.label("No saved encounters yet.");
        return None;
    }

    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for summary in &state.encounters {
                if let Some(row_action) = draw_history_row(ui, summary) {
                    action = Some(row_action);
                }
            }
        });
    action
}

/// One list row: title, subtitle, local date+time, duration, total DPS,
/// player count, and a trailing delete button.
fn draw_history_row(
    ui: &mut egui::Ui,
    summary: &history::EncounterSummary,
) -> Option<HistoryRowAction> {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());

    if !ui.is_rect_visible(rect) {
        return None;
    }

    // The trailing delete button's own hit-test region, inside the row's
    // already-reserved space — checked ahead of the row's own click below so
    // a delete click can never also open the encounter underneath it.
    let delete_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - HISTORY_DELETE_WIDTH - HISTORY_ROW_PADDING,
            rect.top(),
        ),
        egui::vec2(HISTORY_DELETE_WIDTH, rect.height()),
    );
    let delete_response = ui.interact(
        delete_rect,
        ui.id().with(("history_row_delete", summary.id)),
        egui::Sense::click(),
    );

    let painter = ui.painter();
    let center_y = rect.center().y;

    let title_pos = egui::pos2(rect.left() + HISTORY_ROW_PADDING, center_y);
    let title_rect = paint_bold_text(
        painter,
        title_pos,
        egui::Align2::LEFT_CENTER,
        &summary.title,
        FONT_SIZE_ROW,
        TITLE_TEXT_COLOR,
    );
    if let Some(subtitle) = &summary.subtitle {
        paint_text(
            painter,
            egui::pos2(title_rect.right() + HISTORY_ROW_PADDING, center_y),
            egui::Align2::LEFT_CENTER,
            subtitle,
            regular(FONT_SIZE_SUBTITLE),
            SUBTITLE_TEXT_COLOR,
            false,
        );
    }

    let stats = format!(
        "{}    {}    {}/s    {}p",
        history::format_local_time(summary.ended_at_ms),
        history::format_duration(summary.duration_ms),
        fmt_short(summary.total_dps as i64),
        summary.player_count
    );
    paint_text(
        painter,
        egui::pos2(delete_rect.left() - HISTORY_ROW_PADDING, center_y),
        egui::Align2::RIGHT_CENTER,
        &stats,
        regular(FONT_SIZE_SUBTITLE),
        SUBTITLE_TEXT_COLOR,
        false,
    );

    paint_text(
        painter,
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "✕",
        regular(FONT_SIZE_SUBTITLE),
        PILL_VALUE_COLOR,
        false,
    );

    if delete_response.clicked() {
        Some(HistoryRowAction::Delete(summary.id))
    } else if response.clicked() {
        Some(HistoryRowAction::Open(summary.id))
    } else {
        None
    }
}

/// What a click on the list produced.
enum HistoryRowAction {
    Open(i64),
    Delete(i64),
}

/// What a click on the bar produced.
enum HistoryBarAction {
    None,
    Live,
    Back,
    ClearAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ColumnKind, Settings};
    use bpsr_meter::Class;

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

    // -- demo_enabled_from (issue #91) --------------------------------------

    #[test]
    fn demo_enabled_from_unset_is_off() {
        assert!(!demo_enabled_from(None));
    }

    #[test]
    fn demo_enabled_from_empty_is_off() {
        assert!(!demo_enabled_from(Some("")));
    }

    #[test]
    fn demo_enabled_from_explicit_off_values() {
        for value in ["0", "false", "off"] {
            assert!(!demo_enabled_from(Some(value)), "SHINRA_DEMO={value:?}");
        }
    }

    #[test]
    fn demo_enabled_from_on_values_case_insensitive() {
        for value in ["1", "true", "TRUE", "on"] {
            assert!(demo_enabled_from(Some(value)), "SHINRA_DEMO={value:?}");
        }
    }

    #[test]
    fn demo_enabled_from_garbage_is_off() {
        assert!(!demo_enabled_from(Some("banana")));
    }

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

    /// Without demo mode, a fresh `OverlayApp` starts on the ordinary empty
    /// state — no rows, no resolved boss — matching a game that has not
    /// reported an encounter yet.
    #[test]
    fn initial_snapshot_is_empty_when_demo_mode_is_off() {
        let snapshot = initial_snapshot(false);
        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.encounter.boss_name, None);
    }

    // -- DEMO_ROWS Imagine ids (issue #142 test-coverage finding) -----------

    /// Every equipped-Imagine skill id baked into `DEMO_ROWS` must resolve
    /// through `imagine_of_skill_id` to a real curated entry, and that
    /// entry's icon basename must have compiled-in bytes in
    /// `imagines::IMAGINE_ICON_BYTES` — otherwise the demo capture renders
    /// a silent blank placeholder circle instead of the icon it's meant to
    /// show, with nothing failing to say so. This is the test a typo'd id
    /// would have caught.
    #[test]
    fn every_demo_row_imagine_id_resolves_to_a_known_icon_with_bytes() {
        for &(name, _, _, _, _, _, _, ids, _, _) in &DEMO_ROWS {
            for id in ids.into_iter().flatten() {
                let imagine = imagines::imagine_of_skill_id(id)
                    .unwrap_or_else(|| panic!("{name:?}'s Imagine id {id} is not curated"));
                assert!(
                    imagines::IMAGINE_ICON_BYTES
                        .iter()
                        .any(|&(icon, _)| icon == imagine.icon),
                    "{name:?}'s Imagine id {id} resolves to icon {:?}, which has no compiled-in bytes",
                    imagine.icon,
                );
            }
        }
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

    /// Issue #16: the breakdown window is only worth capturing under
    /// `SHINRA_DEMO=1` if every row actually has something to show — a
    /// leftover empty `skills: Vec::new()` (the placeholder T1 left for
    /// this task) would silently produce a blank window.
    #[test]
    fn every_demo_row_has_a_skill_breakdown() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            assert!(
                !row.skills.is_empty(),
                "row {} must have a non-empty skill breakdown",
                row.name
            );
        }
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

    /// Same consistency requirement as damage, for hit counts.
    #[test]
    fn demo_skill_hits_sum_to_the_row_hits() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            let skill_hits_sum: u64 = row.skills.iter().map(|s| s.hits).sum();
            assert_eq!(
                skill_hits_sum, row.hits,
                "row {}'s skill hits must sum to its own hits",
                row.name
            );
        }
    }

    /// A demo capture with every skill reading `Skill #<id>` would be
    /// worthless for eyeballing the breakdown window — issue #16 requires
    /// real ids picked from the vendored, curated skill-name table.
    #[test]
    fn every_demo_skill_id_resolves_to_a_real_name() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            for skill in &row.skills {
                let name = skills::skill_display_name(skill.skill_id);
                assert!(
                    !name.starts_with("Skill #"),
                    "row {}'s skill id {} must resolve to a real name via the generated \
                     table, got fallback {name:?}",
                    row.name,
                    skill.skill_id
                );
            }
        }
    }

    /// Sanity bound on the per-skill numbers themselves, independent of the
    /// row totals: a crit can never be counted more than the skill's own
    /// hits, and the running max crit can never be smaller than the mean
    /// crit it's a max of.
    #[test]
    fn demo_skill_numbers_are_internally_consistent() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            for skill in &row.skills {
                assert!(
                    skill.crit_hits <= skill.hits,
                    "row {}'s skill {} has more crit_hits than hits",
                    row.name,
                    skill.skill_id
                );
                assert!(
                    skill.max_crit as f64 >= skill.avg_crit,
                    "row {}'s skill {} has max_crit {} < avg_crit {}",
                    row.name,
                    skill.skill_id,
                    skill.max_crit,
                    skill.avg_crit
                );
            }
        }
    }

    /// The regression this guards: a future refactor that deletes or
    /// inverts `drain_snapshots`'s `if !self.demo_mode` check would compile
    /// and pass every other test while silently clobbering demo mode's
    /// synthetic capture with the pipeline's real (empty, game-not-running)
    /// snapshot the moment one arrives on the channel.
    #[test]
    fn drain_snapshots_leaves_the_demo_snapshot_intact_when_demo_mode_is_on() {
        let (tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        app.demo_mode = true;
        app.snapshot = demo_snapshot();

        tx_snapshot.send(rows_test_snapshot(2)).unwrap();
        app.drain_snapshots();

        assert_eq!(
            app.snapshot.encounter.boss_name,
            Some("Paradox-Calamity Remnant - Final"),
            "a real snapshot arriving on the channel must not clobber demo mode's capture"
        );
    }

    /// The mirror of the test above: outside demo mode, a real snapshot on
    /// the channel must still win, or the overlay would never leave "No
    /// target" once a real encounter starts.
    #[test]
    fn drain_snapshots_replaces_the_snapshot_when_demo_mode_is_off() {
        let (tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        app.demo_mode = false;

        tx_snapshot.send(rows_test_snapshot(2)).unwrap();
        app.drain_snapshots();

        assert_eq!(app.snapshot.rows.len(), 2);
    }

    /// PR #197 review: the Share clipboard failure is a *transient* banner
    /// — a later Share that works takes it down, and it times out on its
    /// own if none ever does — while the capture-init failure `main.rs`
    /// seeds through `with_status` is permanent and neither path may touch
    /// it. Exercises `OverlayApp`'s own state rather than an egui round
    /// trip, the same way the `pending_screenshot_bound` test does.
    #[test]
    fn a_transient_status_error_clears_itself_but_a_permanent_one_never_does() {
        let new_app = || {
            let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
            let (tx_command, _rx_command) = crossbeam_channel::unbounded();
            let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
            OverlayApp::new(
                rx_snapshot,
                tx_command,
                tx_settings,
                Settings::default(),
                None,
            )
        };
        let failed = || "Copy screenshot failed: the clipboard was busy".to_owned();
        let now = Instant::now();

        // A Share that lands without an error takes the banner down.
        let mut app = new_app();
        app.raise_transient_status(failed(), now);
        assert!(matches!(app.status, StatusLine::Error(_)));
        app.clear_transient_status();
        assert_eq!(
            app.status,
            StatusLine::Ok,
            "a copy that worked must clear the banner a copy that failed raised"
        );

        // With no later Share at all, the timeout alone takes it down.
        let mut app = new_app();
        app.raise_transient_status(failed(), now);
        app.expire_transient_status(now + TRANSIENT_STATUS_LINGER - Duration::from_millis(1));
        assert!(
            matches!(app.status, StatusLine::Error(_)),
            "the banner must stay up until it actually expires"
        );
        app.expire_transient_status(now + TRANSIENT_STATUS_LINGER);
        assert_eq!(
            app.status,
            StatusLine::Ok,
            "the banner must clear itself once TRANSIENT_STATUS_LINGER is up"
        );

        // `with_status` stamps no expiry, so neither path may clear it.
        let permanent = StatusLine::Error("screen capture is unavailable".to_owned());
        let mut app = new_app().with_status(permanent.clone());
        app.clear_transient_status();
        app.expire_transient_status(now + Duration::from_secs(3_600));
        assert_eq!(
            app.status, permanent,
            "the permanent capture-init banner must survive both clearing paths"
        );
    }

    /// Issue #220 (PR #227 review): the export copy runs on a spawned
    /// thread now, so its outcome lands on some later frame — a failure on
    /// the same transient banner the Share failure uses, a success on the
    /// log alone (`StatusLine` has no non-error state to say it with).
    #[test]
    fn a_failed_log_export_raises_a_transient_banner_and_a_successful_one_stays_quiet() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let now = Instant::now();
        let dest = PathBuf::from("exported-logs.log");

        app.tx_log_export.send(Ok(dest.clone())).unwrap();
        app.poll_log_export(now);
        assert_eq!(
            app.status,
            StatusLine::Ok,
            "an export that worked must not raise a banner"
        );

        app.tx_log_export
            .send(Err((dest, "permission denied".to_owned())))
            .unwrap();
        app.poll_log_export(now);
        assert!(
            matches!(&app.status, StatusLine::Error(msg) if msg.contains("permission denied")),
            "a failed export must say why on the banner, not only in the log: {:?}",
            app.status
        );

        // Transient, like the Share failure it borrows the banner from.
        app.expire_transient_status(now + TRANSIENT_STATUS_LINGER);
        assert_eq!(app.status, StatusLine::Ok);
    }

    #[test]
    fn fmt_short_below_thousand_is_plain() {
        assert_eq!(fmt_short(999), "999");
    }

    #[test]
    fn fmt_short_thousands() {
        assert_eq!(fmt_short(1_000), "1.00K");
    }

    #[test]
    fn fmt_short_millions() {
        assert_eq!(fmt_short(1_234_567), "1.23M");
    }

    #[test]
    fn fmt_short_negative() {
        assert_eq!(fmt_short(-1_500), "-1.50K");
    }

    #[test]
    fn fmt_short_billions() {
        assert_eq!(fmt_short(2_500_000_000), "2.50B");
    }

    /// Issue #118: the full table from the issue, verbatim.
    #[test]
    fn fmt_short_issue_118_table() {
        let cases: [(i64, &str); 8] = [
            (1234, "1.23K"),
            (12345, "12.34K"),
            (123456, "123.4K"),
            (1234567, "1.23M"),
            (12345678, "12.34M"),
            (999, "999"),
            (999950, "1000K"),
            (-1500, "-1.50K"),
        ];
        for (input, expected) in cases {
            assert_eq!(fmt_short(input), expected, "fmt_short({input})");
        }
    }

    #[test]
    fn fmt_duration_zero() {
        assert_eq!(fmt_duration(0), "00:00");
    }

    #[test]
    fn fmt_duration_minute_and_seconds() {
        assert_eq!(fmt_duration(65_000), "01:05");
    }

    /// Issue #91: minutes are the leading field and are zero-padded to two
    /// digits below ten, matching the reference render's `02:39` — the
    /// exact value it shows.
    #[test]
    fn fmt_duration_pads_single_digit_minutes() {
        assert_eq!(fmt_duration(159_000), "02:39");
        assert_eq!(fmt_duration(9_000), "00:09");
        assert_eq!(fmt_duration(599_000), "09:59");
        // Ten minutes and up is already two digits — the pad must not add a
        // third.
        assert_eq!(fmt_duration(600_000), "10:00");
    }

    #[test]
    fn fmt_duration_no_hour_rollover() {
        assert_eq!(fmt_duration(3_600_000), "60:00");
        // Issue #91's minute padding must not have introduced one either:
        // three-digit minute counts keep counting, and `120:00` stays the
        // width-budget worst case `the_stat_pills_fit_the_default_window_
        // width` measures.
        assert_eq!(fmt_duration(7_200_000), "120:00");
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
                scene_boss_name: None,
                multi_boss_scene: false,
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

    /// Walks a painted `Shape`, collecting every `Shape::Circle`'s fill
    /// color — how `toggle_button`'s hover wash (`ui.painter().circle_
    /// filled(..., TOGGLE_HOVER_FILL)`) is found in issue #156's
    /// suppression tests, since it's the only circle either button paints.
    fn collect_circle_fills(shape: &egui::Shape, out: &mut Vec<egui::Color32>) {
        match shape {
            egui::Shape::Circle(circle) => out.push(circle.fill),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_circle_fills(s, out);
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
                &mut WindowGesture::default(),
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
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

        /// The box of the largest untextured mesh the header painted — the
        /// background wash's own gradient quad (`gradient_mesh`), which is
        /// the only near-panel-wide mesh in the band carrying no texture of
        /// its own (glyph blits all carry one; every other fill in the
        /// header is a `Shape::Rect`).
        fn gradient_box(&self) -> egui::Rect {
            self.images
                .iter()
                .filter(|(id, rect)| *id == egui::TextureId::default() && rect.is_positive())
                .map(|(_, rect)| *rect)
                .max_by(|a, b| a.area().total_cmp(&b.area()))
                .expect("the header painted no untextured mesh")
        }

        /// The box of the first rect filled with `fill` — how a pill's own
        /// chrome is identified, since each pill kind has its own fill.
        /// `PILL_FILL` is the stat row's, which is what
        /// `a_missing_area_name_does_not_collapse_the_header_or_lift_the_stat_row`
        /// measures the row's position by.
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
                &mut WindowGesture::default(),
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
            );
        });
        for clipped in &output.shapes {
            collect_painted_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        output.drop_without_applying_deltas();
        frame
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

    /// Issue #91: the timer is inset from the panel's left content edge by
    /// `HEADER_STAT_ROW_INSET_X` rather than sitting flush against the
    /// window border the way its old half-pill did. That gap is the whole
    /// reason the timer may keep a *full* oval — nothing crops it.
    ///
    /// Measured on the capsule itself, not on the duration's glyphs: the
    /// pill is the row's leftmost painted mark, and its own left edge is
    /// what the window border would clip, so this pins the exact inset
    /// where an ink-based bound could only assert `>= inset + PILL_PAD_X`.
    /// The panel's content edge is read back off the wash, which
    /// `draw_header` anchors to it at `HEADER_WASH_INSET`.
    ///
    /// And the inset is the *title column's*: the capsule's left edge must
    /// land on the boss name's, so the stat row reads as a third line of the
    /// same block rather than as a row indented by a number of its own.
    #[test]
    fn the_stat_row_ink_is_inset_from_the_panel_edge() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let panel_left = frame.gradient_box().left() - HEADER_WASH_INSET;
        let pill = frame.fill_box(TIMER_PILL_FILL);

        assert!(
            (pill.left() - (panel_left + HEADER_STAT_ROW_INSET_X)).abs() < 0.01,
            "the timer capsule starts at {} — {}pt inside the panel edge at \
             {panel_left}, not {HEADER_STAT_ROW_INSET_X}pt",
            pill.left(),
            pill.left() - panel_left
        );
        assert_eq!(
            HEADER_STAT_ROW_INSET_X,
            HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X,
            "the stat row's inset has drifted off the title column's indent"
        );
        let title = frame.text_box(&encounter_title(&snapshot.encounter));
        assert!(
            (pill.left() - title.left()).abs() < 0.01,
            "the timer capsule starts at {}, the boss name at {} — the stat row \
             is no longer aligned with the title column",
            pill.left(),
            title.left()
        );
        // The duration's own ink then clears the capsule's padding, so a
        // regression that kept the pill inset while un-padding the text
        // still fails here.
        let value = frame.text_box(&fmt_duration(snapshot.duration_ms));
        assert!(
            value.left() >= pill.left() + PILL_PAD_X - 0.01,
            "the duration starts at {} — inside the capsule's {PILL_PAD_X}pt \
             padding from {}",
            value.left(),
            pill.left()
        );
    }

    /// The gutter emblem and the stat row are separated *horizontally*, and
    /// nothing cuts the mark's bottom off to do it.
    ///
    /// Issue #75 clipped the horned mark to the header's *text* rows to keep
    /// its lower blade out of the stat-pill row. The box reaches 14pt past
    /// that band, so the clip did not just stop the blade — it sliced the
    /// diamond's bottom corner off, plainly visible in the composited window.
    /// The row moved right instead (`HEADER_STAT_ROW_INSET_X`), so the mark
    /// may now run its full box down into the stat row's vertical span while
    /// still touching neither the timer's capsule nor its digits.
    ///
    /// Both halves are asserted on one real frame, because either alone is
    /// cheatable: that the painted ink reaches the box's own bottom (the
    /// guard against the clip creeping back up to the text band), and that
    /// it lands on nothing — with the clearance that buys pinned exactly.
    ///
    /// A paint-level check, not a comparison of constants: the box geometry
    /// and the clip are two independent decisions, and only what the frame
    /// actually painted proves both are still right. The panel's own edges
    /// are read back off the wash gradient, which
    /// `the_wash_gradient_spans_the_whole_header_band` independently pins.
    #[test]
    fn the_gutter_emblem_clears_the_stat_row_horizontally_not_by_clipping() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let pill = frame.fill_box(TIMER_PILL_FILL);
        let stat_ink = frame.glyph_boxes(GlyphIcon::Timer)[0]
            .union(frame.text_box(&fmt_duration(snapshot.duration_ms)));
        let wash = frame.gradient_box();
        let panel_min = wash.min - egui::Vec2::splat(HEADER_WASH_INSET);
        let text_band_bottom = panel_min.y + header_text_band_height();
        // Where the box wants to end — 14pt below the text band, inside the
        // stat row's span, and the depth the paint has to actually reach.
        let box_bottom = header_emblem_rect(
            egui::Rect::from_min_size(panel_min, egui::vec2(wash.width(), TITLE_LINE_HEIGHT)),
            header_text_band_height(),
        )
        .bottom();
        assert!(
            box_bottom > text_band_bottom,
            "the emblem box no longer overflows the text band, so the \
             uncut-bottom assertion below proves nothing"
        );

        let emblems = frame.glyph_boxes(GlyphIcon::Emblem);
        assert_eq!(
            emblems.len(),
            2,
            "expected the gutter mark and the wash wallpaper: {emblems:?}"
        );
        // The gutter mark bleeds off the panel's left edge; the wash
        // wallpaper is right-aligned to the wash. Leftmost is the gutter.
        let gutter = emblems
            .iter()
            .copied()
            .min_by(|a, b| a.left().total_cmp(&b.left()))
            .expect("the header painted no emblem");

        // Uncut: the ink ends where the box does, not where the text band
        // does — the diamond keeps its bottom corner.
        assert!(
            (gutter.bottom() - box_bottom).abs() < 0.01,
            "gutter emblem ink stops at {}, short of its box's {box_bottom} \
             (the text band ends at {text_band_bottom}) — its bottom corner is \
             being clipped off again",
            gutter.bottom()
        );
        assert!(
            gutter.bottom() > pill.top(),
            "gutter emblem ink stops at {} — above the stat row starting at {}, \
             so this frame cannot show the two coexisting",
            gutter.bottom(),
            pill.top()
        );

        // …and lands on nothing, despite sharing those rows.
        assert!(
            !gutter.intersects(pill),
            "gutter emblem ink {gutter:?} lands on the timer pill {pill:?}"
        );
        assert!(
            !gutter.intersects(stat_ink),
            "gutter emblem ink {gutter:?} lands on the timer readout {stat_ink:?}"
        );
        // …because of where the row starts, which is the whole mechanism.
        let clearance = pill.left() - gutter.right();
        assert!(
            (clearance - HEADER_TEXT_PAD_X).abs() < 0.01,
            "the stat row starts {clearance}pt right of the emblem, not the \
             {HEADER_TEXT_PAD_X}pt the title column's indent buys it"
        );
        // And it is still a *gutter* decoration horizontally.
        assert!(
            gutter.right() <= panel_min.x + HEADER_GUTTER_WIDTH + 0.01,
            "gutter emblem ink runs to x={}, past the {HEADER_GUTTER_WIDTH}pt gutter",
            gutter.right()
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

    /// Issue #71: the window-control icon-button cluster (Close/Minimize/
    /// Settings) no longer renders directly in the header's stat row —
    /// those actions moved into the chevron's dropdown menu, which paints
    /// nothing until opened. A closed-menu frame must carry no accessible
    /// node labeled for any of the old buttons, only the chevron's own
    /// "Menu" label. Reset is the one exception: issue #82 moved it back
    /// out of the dropdown into the toggle cluster, so it (and the new
    /// Share button) are expected to render directly now.
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
                &mut WindowGesture::default(),
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
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

        for stale in ["Close", "Settings", "Minimize"] {
            assert!(
                !labels.iter().any(|l| l == stale),
                "stale window-control label {stale:?} still renders directly in the header: {labels:?}"
            );
        }
        assert!(
            labels.iter().any(|l| l == "Menu"),
            "expected the chevron's own \"Menu\" label, got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "Reset"),
            "expected the toggle cluster's Reset button, got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "Copy screenshot to clipboard"),
            "expected the toggle cluster's Share button, got {labels:?}"
        );
    }

    /// The reference render shows a total-damage figure alongside the DPS
    /// figure (e.g. "30.10B"), abbreviated with the same `fmt_short` used
    /// everywhere else — `snapshot.total_damage` existed but was never
    /// painted before this change.
    #[test]
    fn draw_header_shows_total_damage_abbreviated() {
        let texts = header_rendered_texts(&header_test_snapshot(30_100_000_000));
        let expected = fmt_short(30_100_000_000);
        assert_eq!(expected, "30.10B");
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
                &mut WindowGesture::default(),
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
            );
            interact_size_y = ui.spacing().interact_size.y;
            rendered_height = ui.min_rect().height();
        });
        output.drop_without_applying_deltas();

        let band = header_band_height(interact_size_y);
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

    /// Issue #91: the timer used to sit flush against the panel's left
    /// border as a half-pill. It is now inset by `HEADER_STAT_ROW_INSET_X`
    /// along with the rest of the stat row, and that inset must be a real
    /// gap — not zero, and not so wide it eats the row's width budget
    /// (`the_stat_pills_fit_the_default_window_width` is the other half of
    /// that bargain).
    ///
    /// The value is not free to drift: it is the title column's own indent,
    /// so the timer sits under the boss name rather than at a margin of its
    /// own — and, since the gutter emblem's box ends at exactly
    /// `HEADER_GUTTER_WIDTH` while hanging into this row's height, that same
    /// sum is what holds the row clear of the mark (see
    /// `the_gutter_emblem_clears_the_stat_row_horizontally_not_by_clipping`).
    #[test]
    fn the_stat_row_is_inset_from_the_left_border() {
        const { assert!(HEADER_STAT_ROW_INSET_X > 0.0) };
        // The same sum `header_text_rect` indents the title and area name by.
        assert_eq!(
            HEADER_STAT_ROW_INSET_X,
            HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X
        );
        assert_eq!(HEADER_STAT_ROW_INSET_X, 36.0);
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
            measure("99.99M/s", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        let dmg = pill_size(
            measure("99.99B", FONT_SIZE_PILL_VALUE),
            PILL_GLYPH_SIDE,
            row_height,
        )
        .x;
        // The toggle cluster (decision 5, issue #82; four buttons as of
        // issue #167): a fixed-width pill, not measured text.
        let toggles = 2.0 * TOGGLE_PAD_X
            + TOGGLE_MOUSE_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLOUD_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLICK_THROUGH_SIDE
            + TOGGLE_GAP
            + TOGGLE_ALWAYS_ON_TOP_SIDE;

        // Three gaps between the outer horizontal's four direct children
        // (timer, dps, dmg, toggle cluster).
        let gaps = 3.0 * 6.0;

        // Issue #91: the row no longer starts flush against the panel's
        // left content edge, so its inset is part of the width it needs.
        let total = HEADER_STAT_ROW_INSET_X + timer + dps + dmg + toggles + gaps;
        assert!(
            total <= default_inner_width(),
            "stat row needs {total}pt but the default window is only {}pt wide",
            default_inner_width()
        );
    }

    /// The title row `title_row_toggles` is drawn into, for a test that
    /// calls it directly rather than through `draw_header`: the top
    /// `TITLE_LINE_HEIGHT` of whatever space the test's `Ui` has, which is
    /// exactly the rect `draw_title_line` allocates and hands over in the
    /// real call path.
    fn test_title_row(ui: &egui::Ui) -> egui::Rect {
        egui::Rect::from_min_size(
            ui.available_rect_before_wrap().min,
            egui::vec2(ui.available_width(), TITLE_LINE_HEIGHT),
        )
    }

    /// Share, Reset (issue #82) and History (issue #186) all paint at
    /// `TOGGLE_ACTIVE_COLOR`: the cluster holds three one-shot actions and
    /// no on/off state since issue #185 took click-through and
    /// always-on-top to the title row. History's tint is
    /// `toggle_state_tint(has_history)`, so an *available* history is what
    /// the active tint means here — see the disabled case below.
    #[test]
    fn the_toggle_cluster_renders_every_button_at_its_state_tint() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let share = icons.glyphs.get(GlyphIcon::Share).unwrap().id();
        let reset = icons.toolbar.get(ToolbarIcon::Reset).unwrap().id();
        let history = icons.glyphs.get(GlyphIcon::History).unwrap().id();

        let tints = |has_history: bool| -> Vec<(egui::TextureId, egui::Color32)> {
            let mut blits = Vec::new();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
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
            for clipped in &output.shapes {
                collect_image_texture_tints(&clipped.shape, &mut blits);
            }
            output.drop_without_applying_deltas();
            blits
        };

        let blits = tints(true);
        let tint_of = |id: egui::TextureId| blits.iter().find(|(t, _)| *t == id).map(|(_, c)| *c);
        for expected in [share, reset, history] {
            assert_eq!(
                tint_of(expected),
                Some(TOGGLE_ACTIVE_COLOR),
                "{expected:?} was not blitted at TOGGLE_ACTIVE_COLOR: {blits:?}"
            );
        }

        // Issue #186: the disabled state the old `draw_header_menu` item
        // carried survives the move as the dim "off" tint.
        let disabled = tints(false);
        assert_eq!(
            disabled
                .iter()
                .find(|(t, _)| *t == history)
                .map(|(_, c)| *c),
            Some(TOGGLE_OFF_COLOR),
            "History must blit at TOGGLE_OFF_COLOR with no history thread: {disabled:?}"
        );
    }

    /// All three toggle-cluster controls (Share, Reset — issue #82 — and
    /// History — issue #186) are real buttons: the tree must expose exactly
    /// three `Button` accesskit nodes, with no leftover inert decoration and
    /// none of the two controls issue #185 moved out to the title row.
    #[test]
    fn the_toggle_cluster_exposes_three_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        output.drop_without_applying_deltas();

        let button_count = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Button)
            .count();
        assert_eq!(
            button_count, 3,
            "expected Share, Reset and History to each expose a Button role, got {button_count}"
        );
    }

    /// PR #197 review: with no history thread the History button must be
    /// *genuinely* disabled, not merely dim and click-gated — accesskit has
    /// to publish `enabled: false`, or a screen-reader user hears a usable
    /// "History: unavailable" button, activates it, and nothing happens.
    #[test]
    fn the_history_button_reports_itself_disabled_without_history() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let disabled = |has_history: bool, label: &str| -> bool {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
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
            let update = output
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            output.drop_without_applying_deltas();
            // A direct `==` for the same target-dependent reason
            // `accessible_rect_for_label` spells out.
            update
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|s| s == label))
                .map(|(_, node)| node.is_disabled())
                .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"))
        };

        assert!(
            disabled(false, "History: unavailable"),
            "History must report enabled: false with no history thread"
        );
        assert!(
            !disabled(true, "History"),
            "History must stay enabled when there is a history thread"
        );
    }

    /// Mirrors `the_history_button_reports_itself_disabled_without_history`
    /// for Share (PR #225 review of issue #219): every existing
    /// `toggle_cluster` test call site passes `share_active = true`, so
    /// nothing verified the button is genuinely disabled — not merely
    /// dim — when it's false. Same contract as History: accesskit must
    /// publish `enabled: false`, and a click landing on the button's rect
    /// must not fire a capture.
    #[test]
    fn the_share_button_reports_itself_disabled_when_inactive() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let disabled = |share_active: bool, label: &str| -> bool {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                toggle_cluster(
                    ui,
                    &tx_command,
                    &icons,
                    false,
                    share_active,
                    true,
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
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|s| s == label))
                .map(|(_, node)| node.is_disabled())
                .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"))
        };

        assert!(
            disabled(false, "Copy screenshot to clipboard: unavailable"),
            "Share must report enabled: false when share_active is false"
        );
        assert!(
            !disabled(true, "Copy screenshot to clipboard"),
            "Share must stay enabled when share_active is true"
        );

        let label = "Copy screenshot to clipboard: unavailable";
        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, false, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, label).center();
        layout.drop_without_applying_deltas();

        let mut screenshot_requested = true;
        let output = ctx.run_ui(click_at(pos), |ui| {
            screenshot_requested =
                toggle_cluster(ui, &tx_command, &icons, false, false, true, &mut false);
        });
        output.drop_without_applying_deltas();
        assert!(
            !screenshot_requested,
            "clicking a disabled Share button must not fire a capture"
        );
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

    /// Issue #185: the title row's own pill holds click-through and
    /// always-on-top, each at `toggle_state_tint` of its default `Settings`
    /// value — off (`TOGGLE_OFF_COLOR`) for click-through, on
    /// (`TOGGLE_ACTIVE_COLOR`) for always-on-top — and exposes both as real
    /// buttons.
    #[test]
    fn the_title_row_pill_renders_both_toggles_at_their_state_tint() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let click_through = icons.glyphs.get(GlyphIcon::MouseOff).unwrap().id();
        let always_on_top = icons.glyphs.get(GlyphIcon::Pin).unwrap().id();

        let mut blits = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        for clipped in &output.shapes {
            collect_image_texture_tints(&clipped.shape, &mut blits);
        }
        let update = output
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        output.drop_without_applying_deltas();

        let tint_of = |id: egui::TextureId| blits.iter().find(|(t, _)| *t == id).map(|(_, c)| *c);
        assert_eq!(
            tint_of(click_through),
            Some(TOGGLE_OFF_COLOR),
            "click-through defaults to off, so it must blit at TOGGLE_OFF_COLOR: {blits:?}"
        );
        assert_eq!(
            tint_of(always_on_top),
            Some(TOGGLE_ACTIVE_COLOR),
            "always-on-top defaults to on, so it must blit at TOGGLE_ACTIVE_COLOR: {blits:?}"
        );
        let button_count = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Button)
            .count();
        assert_eq!(
            button_count, 2,
            "expected click-through and always-on-top to each expose a Button role, got {button_count}"
        );
    }

    /// Clicking the click-through button flips `Settings::click_through`,
    /// tells the platform layer (`platform::set_click_through` — issue
    /// #167 rehash; a no-op stub off-Windows, so not independently
    /// observable from this test), and persists the change (issue #167).
    #[test]
    fn clicking_click_through_toggles_and_persists_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        assert!(!settings.click_through);
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, "Click-through: off").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(pos), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        output.drop_without_applying_deltas();

        assert!(settings.click_through, "the click must flip the flag on");
        let persisted = rx_settings.try_recv().expect("the click must persist");
        assert!(persisted.click_through);
    }

    /// Clicking the always-on-top button flips `Settings::always_on_top`,
    /// sends `ViewportCommand::WindowLevel(Normal)` (it starts on, so a
    /// click turns it off), and persists the change (issue #167).
    #[test]
    fn clicking_always_on_top_toggles_and_persists_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let mut settings = Settings::default();
        assert!(settings.always_on_top);
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let pos = accessible_rect_for_label(&update, "Always on top: on").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(pos), |ui| {
            let title_row = test_title_row(ui);
            title_row_toggles(
                ui,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                &icons,
                title_row,
                false,
            );
        });
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert!(!settings.always_on_top, "the click must flip the flag off");
        assert!(
            commands.iter().any(|cmd| matches!(
                cmd,
                egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal)
            )),
            "the click must request WindowLevel(Normal): {commands:?}"
        );
        let persisted = rx_settings.try_recv().expect("the click must persist");
        assert!(!persisted.always_on_top);
    }

    /// Clicking the Share button fires the screenshot capture request; the
    /// clipboard write itself happens later, off `Event::Screenshot` (see
    /// `handle_screenshot_events_routes_a_screenshot_event_to_the_writer`).
    #[test]
    fn clicking_share_sends_a_screenshot_viewport_command() {
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
        let share_pos = accessible_rect_for_label(&update, "Copy screenshot to clipboard").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(share_pos), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert!(
            commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::Screenshot(_))),
            "Share must request a screenshot: {commands:?}"
        );
        assert!(
            rx_command.try_recv().is_err(),
            "Share must not also send a UiCommand"
        );
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

    /// Issue #156: the pointer is necessarily still over the Share button
    /// on the frame right after the click that started a screenshot
    /// capture (`ViewportCommand::Screenshot` captures "the next frame
    /// after" the request, per its own doc comment), so `toggle_button`'s
    /// hover circle would otherwise paint straight into the captured
    /// image. `capturing: true` must suppress it — for every button in the
    /// cluster, not just Share, since one guard covers the whole row.
    ///
    /// The `capturing: false` half is a sanity check on the test itself:
    /// without it, a `toggle_button` that suppressed its hover fill
    /// unconditionally would pass the `true` half for the wrong reason.
    #[test]
    fn toggle_button_suppresses_its_hover_fill_while_a_screenshot_capture_is_in_flight() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, "Copy screenshot to clipboard").center();
        layout.drop_without_applying_deltas();

        let hover_input = || egui::RawInput {
            events: vec![egui::Event::PointerMoved(share_pos)],
            ..Default::default()
        };

        let fills_while_hovered = |capturing: bool| -> Vec<egui::Color32> {
            let output = ctx.run_ui(hover_input(), |ui| {
                toggle_cluster(ui, &tx_command, &icons, capturing, true, true, &mut false);
            });
            let mut fills = Vec::new();
            for clipped in &output.shapes {
                collect_circle_fills(&clipped.shape, &mut fills);
            }
            output.drop_without_applying_deltas();
            fills
        };

        let normal = fills_while_hovered(false);
        assert!(
            normal.contains(&TOGGLE_HOVER_FILL),
            "sanity check: hovering Share with no capture in flight must \
             still paint the hover fill: {normal:?}"
        );

        let while_capturing = fills_while_hovered(true);
        assert!(
            !while_capturing.contains(&TOGGLE_HOVER_FILL),
            "the hover fill must be suppressed while a Share screenshot \
             capture is in flight: {while_capturing:?}"
        );
    }

    /// `toggle_button`'s doc comment and `capturing`'s call sites both claim
    /// suppression covers the tooltip as well as the hover circle — but the
    /// hover-fill test above only ever looks at painted circles, so it
    /// cannot catch a regression in the separate `if capturing { response }
    /// else { response.on_hover_text(label) }` branch that actually gates
    /// the tooltip. This drives that branch directly: `response.
    /// on_hover_text(label)` paints the label text itself (`ui.add(Label::
    /// new(text))` inside `Tooltip`'s contents), so its presence or absence
    /// in the painted `Shape::Text`s is a direct behavioral signature of
    /// which branch ran.
    ///
    /// `tooltip_delay` is zeroed so the tooltip shows on the very same
    /// frame the pointer arrives, instead of needing a run of frames with a
    /// stationary pointer to clear egui's real (0.5s) delay — this test
    /// only cares which branch `toggle_button` took, not egui's own hover
    /// timing.
    ///
    /// The `capturing: false` half is the same kind of sanity check as the
    /// hover-fill test's: without it, a harness that just can't produce a
    /// tooltip at all (wrong style, wrong input shape, ...) would pass the
    /// `true` half for the wrong reason.
    #[test]
    fn toggle_button_suppresses_its_tooltip_while_a_screenshot_capture_is_in_flight() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        ctx.global_style_mut(|style| style.interaction.tooltip_delay = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        const SHARE_LABEL: &str = "Copy screenshot to clipboard";

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons, false, true, true, &mut false);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, SHARE_LABEL).center();
        layout.drop_without_applying_deltas();

        let hover_input = || egui::RawInput {
            events: vec![egui::Event::PointerMoved(share_pos)],
            ..Default::default()
        };

        let texts_while_hovered = |capturing: bool| -> Vec<String> {
            let output = ctx.run_ui(hover_input(), |ui| {
                toggle_cluster(ui, &tx_command, &icons, capturing, true, true, &mut false);
            });
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                collect_text_shapes(&clipped.shape, &mut texts);
            }
            output.drop_without_applying_deltas();
            texts
        };

        // Same reason the menu-chevron test (`accessible_rect_for_label`'s
        // caller) needs a settle frame: `Popup::show` runs a just-opened
        // Area through a `sizing_pass` with no prior measured size, so its
        // first frame doesn't paint the same way every later frame does.
        // This frame only arms the tooltip; nothing here is asserted on.
        texts_while_hovered(false);

        let normal = texts_while_hovered(false);
        assert!(
            normal.iter().any(|text| text == SHARE_LABEL),
            "sanity check: hovering Share with no capture in flight must \
             still show its tooltip: {normal:?}"
        );

        let while_capturing = texts_while_hovered(true);
        assert!(
            !while_capturing.iter().any(|text| text == SHARE_LABEL),
            "the tooltip must be suppressed while a Share screenshot \
             capture is in flight: {while_capturing:?}"
        );
    }

    /// `handle_screenshot_events` is the routing half of the Share round
    /// trip: it must find a synthesized `Event::Screenshot` in this frame's
    /// input and hand its image to the writer, without needing a live
    /// window or a real clipboard.
    #[test]
    fn handle_screenshot_events_routes_a_screenshot_event_to_the_writer() {
        let ctx = egui::Context::default();
        let image = std::sync::Arc::new(egui::ColorImage::filled(
            [2, 2],
            egui::Color32::from_rgb(1, 2, 3),
        ));
        let input = egui::RawInput {
            events: vec![egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                user_data: egui::UserData::default(),
                image: image.clone(),
            }],
            ..Default::default()
        };

        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
        let output = ctx.run_ui(input, |_ui| {
            handle_screenshot_events(&ctx, |img| written.push(img));
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            written.len(),
            1,
            "the screenshot event must reach the writer exactly once"
        );
        assert_eq!(written[0].size, [2, 2]);
    }

    /// A frame with no `Event::Screenshot` in it must never call the
    /// writer — otherwise every idle frame would overwrite the clipboard.
    #[test]
    fn handle_screenshot_events_does_nothing_without_a_screenshot_event() {
        let ctx = egui::Context::default();
        let mut called = false;
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            handle_screenshot_events(&ctx, |_img| called = true);
        });
        output.drop_without_applying_deltas();

        assert!(
            !called,
            "the writer must not run without a Screenshot event"
        );
    }

    // -- Share screenshot cropping (issue #96) -------------------------------

    /// Issue #96 (PR #98 review): `pending_screenshot_bound` must be pinned
    /// to the row count as of the frame the Share click fired on — reading
    /// `self.snapshot` fresh when the async `Event::Screenshot` reply lands
    /// would crop against a row count that no longer matches the pixels
    /// actually captured (a player joining or dropping mid-encounter is
    /// routine during a real round trip). Exercises `OverlayApp`'s own
    /// field directly, not just the pure crop helpers, so a future
    /// refactor that reintroduces a reply-time read would be caught here —
    /// no egui round trip needed.
    #[test]
    fn pending_screenshot_bound_survives_a_later_snapshot_row_count_change() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );

        // What `OverlayApp::ui` does at the end of the frame the Share
        // click fired on: stash *that* frame's row bound, computed from
        // *that* frame's row count (3 rows).
        app.snapshot = rows_test_snapshot(3);
        let request_time_bound =
            rows_content_bottom_y(50.0, app.snapshot.rows.len(), ROW_HEIGHT, 500.0);
        app.pending_screenshot_bound = Some(request_time_bound);

        // A player joins mid-encounter before the async reply lands — the
        // row count changes underneath the pending request.
        app.snapshot = rows_test_snapshot(8);

        assert_eq!(
            app.pending_screenshot_bound,
            Some(request_time_bound),
            "the pending bound must stay pinned to the request-time row \
             count, not drift with the live snapshot"
        );
    }

    /// Fewer/shorter populated rows than the scroll area's allocated height
    /// must trim the bound to right after the last row, not the scroll
    /// area's full height — no trailing empty background.
    #[test]
    fn rows_content_bottom_y_trims_trailing_empty_space() {
        let bottom = rows_content_bottom_y(100.0, 3, 30.0, 500.0);
        assert_eq!(bottom, 100.0 + 3.0 * 30.0);
    }

    /// More populated-row content than the scroll area shows (scrolled
    /// off) must not push the bound past what the scroll area actually
    /// rendered — that content was clipped out of the frame itself.
    #[test]
    fn rows_content_bottom_y_clamps_to_the_scroll_area_when_rows_overflow_it() {
        let bottom = rows_content_bottom_y(100.0, 50, 30.0, 200.0);
        assert_eq!(bottom, 100.0 + 200.0);
    }

    /// Zero populated rows must not add any height past the chrome.
    #[test]
    fn rows_content_bottom_y_is_just_the_chrome_with_no_rows() {
        assert_eq!(rows_content_bottom_y(100.0, 0, 30.0, 500.0), 100.0);
    }

    /// The common case: converts a points bound to pixels via
    /// `pixels_per_point` and stays within the image.
    #[test]
    fn screenshot_crop_height_px_scales_by_pixels_per_point() {
        assert_eq!(screenshot_crop_height_px(200.0, 2.0, 1000), 400);
    }

    /// A stale bound taller than the actual captured image (e.g. the
    /// window shrank between the frame that computed the bound and the
    /// later frame the screenshot reply landed on) must clamp to the
    /// image's real height, never index past it.
    #[test]
    fn screenshot_crop_height_px_clamps_to_the_image_height() {
        assert_eq!(screenshot_crop_height_px(10_000.0, 1.0, 600), 600);
    }

    /// A non-positive bound (e.g. no bound stashed yet, so the crop falls
    /// back to `0.0`) must crop to nothing rather than underflow.
    #[test]
    fn screenshot_crop_height_px_floors_a_non_positive_bound_to_zero() {
        assert_eq!(screenshot_crop_height_px(0.0, 1.0, 600), 0);
        assert_eq!(screenshot_crop_height_px(-5.0, 1.0, 600), 0);
    }

    /// PR #98 review (clippy `neg_cmp_op_on_partial_ord`): a NaN bound or
    /// scale must floor to zero too, matching the original `!(x > 0.0)`
    /// behavior for every input (NaN comparisons are always `false`, so the
    /// negated form treated NaN as "not positive" — the rewritten
    /// `partial_cmp`-based check must preserve exactly that, not just the
    /// non-NaN cases).
    #[test]
    fn screenshot_crop_height_px_floors_a_nan_bound_or_scale_to_zero() {
        assert_eq!(screenshot_crop_height_px(f32::NAN, 1.0, 600), 0);
        assert_eq!(screenshot_crop_height_px(200.0, f32::NAN, 600), 0);
        assert_eq!(screenshot_crop_height_px(f32::NAN, f32::NAN, 600), 0);
    }

    /// Issue #96 (PR #98 review): the crop must be driven by the row count
    /// as of the frame the screenshot was actually *requested* on, not
    /// whatever row count is current by the time the async reply is
    /// handled — the row count is live combat data (a player joining or
    /// dropping mid-encounter is routine) and can change during the round
    /// trip. This exercises the pure crop helpers directly with two
    /// different row counts standing in for "request time" and "reply
    /// time", proving they produce different — and the request-time one is
    /// the one that must be used.
    #[test]
    fn crop_uses_the_bound_captured_at_request_time_even_if_the_row_count_later_changes() {
        let rows_top = 50.0;
        let row_height = 20.0;
        let rows_area_height = 500.0;
        let pixels_per_point = 1.0;

        // The frame the Share click fired on: 3 populated rows.
        let request_time_bound = rows_content_bottom_y(rows_top, 3, row_height, rows_area_height);

        // By the time the async reply lands, a player joined mid-encounter
        // and the row count grew — recomputing from *that* would produce a
        // different bound than what was actually captured.
        let reply_time_bound = rows_content_bottom_y(rows_top, 8, row_height, rows_area_height);
        assert_ne!(
            request_time_bound, reply_time_bound,
            "the test setup must actually exercise a row-count change"
        );

        let image = crop_test_image(4, 200);
        let cropped = crop_screenshot_to_rows(&image, request_time_bound, pixels_per_point);

        let expected_height =
            screenshot_crop_height_px(request_time_bound, pixels_per_point, image.size[1]);
        assert_eq!(cropped.size, [4, expected_height]);
        assert_ne!(
            cropped.size[1],
            screenshot_crop_height_px(reply_time_bound, pixels_per_point, image.size[1]),
            "must not crop as if it were computed from the reply-time row count"
        );
    }

    fn crop_test_image(width: usize, height: usize) -> std::sync::Arc<egui::ColorImage> {
        let pixels = (0..width * height)
            .map(|i| egui::Color32::from_gray((i % 256) as u8))
            .collect();
        std::sync::Arc::new(egui::ColorImage::new([width, height], pixels))
    }

    /// The primary case: trims the image down to the rows' bottom edge,
    /// keeping every pixel above it and dropping every row below.
    #[test]
    fn crop_screenshot_to_rows_trims_below_the_bound() {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 6.0, 1.0);
        assert_eq!(cropped.size, [4, 6]);
        assert_eq!(cropped.pixels, image.pixels[..4 * 6]);
    }

    /// When the computed bound reaches or exceeds the image's actual
    /// height (content overflows the scroll area, or a stale bound), the
    /// full capture is kept unmodified rather than degenerately cropped.
    #[test]
    fn crop_screenshot_to_rows_keeps_the_full_image_when_the_bound_is_not_smaller() {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 1000.0, 1.0);
        assert_eq!(cropped.size, image.size);
        assert_eq!(cropped.pixels, image.pixels);
    }

    /// Issue #82: a zero (or negative) row bound — e.g. the "no bound
    /// pinned yet" fallback `unwrap_or(0.0)` at the `crop_screenshot_to_rows`
    /// call site — used to make `screenshot_crop_height_px` return `0`,
    /// which `crop_screenshot_to_rows` then turned into a genuinely
    /// zero-height `ColorImage`. arboard rejects a zero-height image with
    /// `Error::ConversionFailure`, which is exactly the clipboard write
    /// failure reported in issue #82: every single Share click failed. A
    /// full, uncropped screenshot is a far better failure mode than no
    /// screenshot at all, so a degenerate crop height must fall back to the
    /// full image instead of an empty one.
    #[test]
    fn crop_screenshot_to_rows_falls_back_to_the_full_image_when_the_bound_is_non_positive() {
        let image = crop_test_image(4, 10);

        let cropped_zero = crop_screenshot_to_rows(&image, 0.0, 1.0);
        assert_eq!(cropped_zero.size, image.size);
        assert_eq!(cropped_zero.pixels, image.pixels);

        let cropped_negative = crop_screenshot_to_rows(&image, -5.0, 1.0);
        assert_eq!(cropped_negative.size, image.size);
        assert_eq!(cropped_negative.pixels, image.pixels);
    }

    /// Same degenerate-crop failure mode as above (issue #82), but driven
    /// by a non-positive `pixels_per_point` instead of a non-positive
    /// bound — both inputs feed the same `screenshot_crop_height_px` zero
    /// fallback, so both must produce the full image rather than a
    /// zero-height one.
    #[test]
    fn crop_screenshot_to_rows_falls_back_to_the_full_image_when_pixels_per_point_is_non_positive()
    {
        let image = crop_test_image(4, 10);
        let cropped = crop_screenshot_to_rows(&image, 6.0, 0.0);
        assert_eq!(cropped.size, image.size);
        assert_eq!(cropped.pixels, image.pixels);
    }

    // -- pending screenshot bound take/consume (issue #82) -------------------

    /// Issue #82 (async round-trip regression): a frame with no screenshot
    /// event must leave whatever bound is pending untouched — the previous
    /// code unconditionally `take()`d `pending_screenshot_bound` every
    /// single frame regardless of whether that frame actually handled a
    /// screenshot reply, so the bound stashed on the request frame was
    /// discarded on the very next idle frame, long before the async
    /// `ViewportCommand::Screenshot` reply could ever land. By the time the
    /// reply arrived, the bound was already `None`, and the crop fell back
    /// to `unwrap_or(0.0)` — a zero row bound that produced a degenerate,
    /// zero-height clipboard image on every single Share click.
    #[test]
    fn take_pending_screenshot_bound_leaves_the_pending_bound_untouched_when_no_event_landed() {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(Some(123.0), false);
        assert_eq!(
            new_pending,
            Some(123.0),
            "a frame without a screenshot event must not consume the pending bound"
        );
        // Nothing reads `bound_to_use` when no event landed, but it must
        // still be a well-defined, harmless value.
        let _ = bound_to_use;
    }

    /// The frame that actually handles a screenshot event must consume the
    /// pending bound — using it for the crop and resetting the field to
    /// `None` so a later, unrelated frame can never reuse a stale value.
    #[test]
    fn take_pending_screenshot_bound_consumes_the_bound_when_an_event_lands() {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(Some(123.0), true);
        assert_eq!(bound_to_use, 123.0);
        assert_eq!(
            new_pending, None,
            "a landed event must consume (reset) the pending bound"
        );
    }

    /// A screenshot event landing with no bound ever pinned (e.g. the round
    /// trip somehow bypassed the request-time stash) must fall back to
    /// `0.0`, matching the previous `unwrap_or(0.0)` behavior, rather than
    /// panicking.
    #[test]
    fn take_pending_screenshot_bound_falls_back_to_zero_when_an_event_lands_with_no_pending_bound()
    {
        let (bound_to_use, new_pending) = take_pending_screenshot_bound(None, true);
        assert_eq!(bound_to_use, 0.0);
        assert_eq!(new_pending, None);
    }

    // -- screenshot_capture_guard (issue #156) --------------------------

    /// The guard must switch on the instant a Share click fires a new
    /// request — regardless of what it held before — so `toggle_cluster`'s
    /// suppression is armed before the very next frame, the one
    /// `ViewportCommand::Screenshot` actually captures.
    #[test]
    fn screenshot_capture_guard_switches_on_the_frame_a_request_is_fired() {
        assert!(screenshot_capture_guard(false, true, false));
        // A request firing on the same frame an old capture's reply lands
        // must still leave the guard set for the new capture in flight,
        // not clear it because a reply also landed this frame.
        assert!(screenshot_capture_guard(true, true, true));
    }

    /// A frame with neither a new request nor a landed reply must leave
    /// the guard exactly as it found it — this is what keeps the
    /// suppression alive across however many idle frames the async round
    /// trip takes.
    #[test]
    fn screenshot_capture_guard_holds_steady_on_an_idle_frame() {
        assert!(!screenshot_capture_guard(false, false, false));
        assert!(screenshot_capture_guard(true, false, false));
    }

    /// The frame the `Event::Screenshot` reply lands on clears the guard —
    /// suppression is only needed until the capture actually happens, and
    /// by the time the reply arrives it already has.
    #[test]
    fn screenshot_capture_guard_clears_when_the_reply_lands() {
        assert!(!screenshot_capture_guard(true, false, true));
    }

    /// The property the issue calls out explicitly: `ViewportCommand::
    /// Screenshot` captures "the next frame after" the one that sends it,
    /// and the `Event::Screenshot` reply can land any number of frames
    /// after *that* — so a guard that only covered the request frame would
    /// be a silent no-op (the request frame is never the one in the
    /// screenshot). Driving `screenshot_capture_guard` across a simulated
    /// frame sequence — request, the captured frame right after it, one
    /// more idle frame while the reply is still in flight, then the reply
    /// landing — proves the guard stays set for every frame in between,
    /// not just the click.
    #[test]
    fn screenshot_capture_guard_stays_set_through_the_captured_frame_and_every_frame_until_the_reply_lands()
     {
        let mut capturing = false;

        // Frame 0: the Share click fires the request. This frame itself is
        // never captured (`ViewportCommand::Screenshot` captures the frame
        // *after* this one), but the guard must already be set entering
        // the next frame.
        capturing = screenshot_capture_guard(capturing, true, false);
        assert!(
            capturing,
            "the guard must be set the instant the request fires"
        );

        // Frame 1: this is the frame that actually gets captured. No new
        // request, no reply yet — the guard must still be set here, or the
        // suppression never covers the one frame that matters.
        capturing = screenshot_capture_guard(capturing, false, false);
        assert!(
            capturing,
            "the guard must still be set on the captured frame, one frame after the click"
        );

        // Frame 2: still waiting on the async `Event::Screenshot` reply.
        capturing = screenshot_capture_guard(capturing, false, false);
        assert!(
            capturing,
            "the guard must stay set for every frame the reply hasn't landed on yet"
        );

        // Frame 3: the reply finally lands — the guard clears.
        capturing = screenshot_capture_guard(capturing, false, true);
        assert!(!capturing, "the guard must clear once the reply has landed");
    }

    /// `advance_screenshot_capture_wait` must reset to `0` on the exact
    /// same two triggers that reset `screenshot_capturing` itself — a new
    /// request, or a landed reply — so the two pure functions can never
    /// fall out of step (a stale non-zero count surviving a request/reply
    /// would let `screenshot_capture_timed_out` fire early on the next
    /// capture).
    #[test]
    fn advance_screenshot_capture_wait_resets_on_request_or_reply_and_counts_otherwise() {
        assert_eq!(advance_screenshot_capture_wait(0, true, false), 0);
        assert_eq!(advance_screenshot_capture_wait(5, true, false), 0);
        assert_eq!(advance_screenshot_capture_wait(5, false, true), 0);
        assert_eq!(advance_screenshot_capture_wait(5, true, true), 0);
        assert_eq!(advance_screenshot_capture_wait(0, false, false), 1);
        assert_eq!(advance_screenshot_capture_wait(5, false, false), 6);
    }

    /// `screenshot_capture_timed_out` is a plain threshold check against
    /// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES` — false below it, true at and
    /// past it (never lands, just keeps latching the guard closed on every
    /// later idle frame too).
    #[test]
    fn screenshot_capture_timed_out_trips_at_the_threshold() {
        assert!(!screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES - 1
        ));
        assert!(screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES
        ));
        assert!(screenshot_capture_timed_out(
            SCREENSHOT_CAPTURE_TIMEOUT_FRAMES + 1
        ));
    }

    /// The actual bug this fallback exists for: a reply that never lands
    /// at all (the queued screenshot silently dropped somewhere in
    /// egui-wgpu's `Painter::paint_and_update_textures` — see
    /// `SCREENSHOT_CAPTURE_TIMEOUT_FRAMES`'s doc comment). Driving the
    /// three pure functions together, frame by frame, exactly as
    /// `OverlayApp::ui` does — `event_landed` is always `false` here, on
    /// every single frame — must still clear the guard once the timeout is
    /// reached, or `screenshot_capturing` would latch `true` forever and
    /// permanently suppress the toggle cluster's hover fill and tooltip.
    #[test]
    fn screenshot_capture_guard_clears_via_the_timeout_fallback_when_the_reply_never_lands() {
        let mut capturing = false;
        let mut frames_waited = 0u32;

        // Frame 0: the Share click fires the request.
        capturing = screenshot_capture_guard(capturing, true, false);
        frames_waited = advance_screenshot_capture_wait(frames_waited, true, false);
        assert!(capturing);

        // Every frame after that: the reply never lands. The guard must
        // still be set right up until the timeout, and clear exactly once
        // it's reached — never later, and never by some other means.
        for frame in 0..SCREENSHOT_CAPTURE_TIMEOUT_FRAMES {
            let timed_out = screenshot_capture_timed_out(frames_waited);
            assert!(
                !timed_out,
                "must not time out before frame {SCREENSHOT_CAPTURE_TIMEOUT_FRAMES}: at frame {frame}"
            );
            assert!(
                capturing,
                "the guard must stay set while waiting for a reply that hasn't timed out yet: frame {frame}"
            );
            capturing = screenshot_capture_guard(capturing, false, timed_out);
            frames_waited = advance_screenshot_capture_wait(frames_waited, false, timed_out);
        }

        assert!(
            screenshot_capture_timed_out(frames_waited),
            "the wait must have reached the timeout by now"
        );
        let timed_out = screenshot_capture_timed_out(frames_waited);
        capturing = screenshot_capture_guard(capturing, false, timed_out);
        assert!(
            !capturing,
            "the guard must clear via the timeout fallback when the reply never lands"
        );
    }

    // -- toggle_cluster: click-through / always-on-top (issue #167) --------

    #[test]
    fn toggle_state_tint_is_active_color_when_on() {
        assert_eq!(toggle_state_tint(true), TOGGLE_ACTIVE_COLOR);
    }

    #[test]
    fn toggle_state_tint_is_off_color_when_off() {
        assert_eq!(toggle_state_tint(false), TOGGLE_OFF_COLOR);
    }

    /// The tray escape hatch: a "Turn off click-through" request must force
    /// `click_through` off, regardless of which state it was in — see
    /// `click_through_after_tray_request`'s doc comment.
    #[test]
    fn click_through_after_tray_request_forces_it_off() {
        assert!(!click_through_after_tray_request(false, true));
        assert!(!click_through_after_tray_request(true, true));
    }

    /// An idle frame (no tray request) must leave `click_through` exactly
    /// as it found it, in either state — this is what keeps the poll from
    /// fighting the toggle button's own clicks on every other frame.
    #[test]
    fn click_through_after_tray_request_holds_steady_with_no_request() {
        assert!(!click_through_after_tray_request(false, false));
        assert!(click_through_after_tray_request(true, false));
    }

    // -- toggle_cluster: Share availability (issue #219) --------------------

    /// The live view is a DPS row surface — Share must be clickable.
    #[test]
    fn share_active_for_view_is_true_on_live() {
        assert!(share_active_for_view(&OverlayView::Live));
    }

    /// The bare history list (no encounter open) is not a row surface at
    /// all — clicking Share there would capture the list of past
    /// encounters, not a fight, so it must be inactive.
    #[test]
    fn share_active_for_view_is_false_on_the_bare_history_list() {
        let view = OverlayView::History(Box::default());
        assert!(!share_active_for_view(&view));
    }

    /// A specific historical encounter renders through the same `draw_rows`
    /// a live fight uses — Share must be just as active there as on Live.
    #[test]
    fn share_active_for_view_is_true_when_a_historical_encounter_is_open() {
        let view = OverlayView::History(Box::new(HistoryUi {
            open: Some(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(3),
            }),
            ..HistoryUi::default()
        }));
        assert!(share_active_for_view(&view));
    }

    // -- screenshot_row_count (issue #219) -----------------------------------

    /// Live view: the crop must use the live snapshot's own row count.
    #[test]
    fn screenshot_row_count_uses_the_live_count_on_live() {
        assert_eq!(screenshot_row_count(&OverlayView::Live, 5), 5);
    }

    /// A historical encounter is open: the crop must use *its* row count,
    /// not the live snapshot's (which may differ, or even be zero if the
    /// live encounter has ended) — this is the second half of issue #219's
    /// crop bug.
    #[test]
    fn screenshot_row_count_uses_the_open_encounters_count_when_history_is_open() {
        let view = OverlayView::History(Box::new(HistoryUi {
            open: Some(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(9),
            }),
            ..HistoryUi::default()
        }));
        assert_eq!(screenshot_row_count(&view, 5), 9);
    }

    /// The bare history list has no open encounter — Share is inactive
    /// there (see `share_active_for_view`), so the fallback value is never
    /// actually used for a real crop, but must still be well-defined.
    #[test]
    fn screenshot_row_count_falls_back_to_the_live_count_on_the_bare_history_list() {
        let view = OverlayView::History(Box::default());
        assert_eq!(screenshot_row_count(&view, 5), 5);
    }

    /// PR #225 review of issue #219: a "Back to Live" click and a Share
    /// click landing in the same frame (`back_to_live` and the crop-bound
    /// use both true) must still crop against the historical encounter
    /// `draw_history` painted this frame, not the `OverlayView::Live` the
    /// same frame's reset is about to produce.
    #[test]
    fn resolve_screenshot_row_count_uses_the_painted_view_even_when_back_to_live_fires_this_frame()
    {
        let mut view = OverlayView::History(Box::new(HistoryUi {
            open: Some(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(9),
            }),
            ..HistoryUi::default()
        }));

        let row_count = resolve_screenshot_row_count(&mut view, true, 5);

        assert_eq!(
            row_count, 9,
            "the crop bound must use the historical encounter's row count, not the live snapshot's"
        );
        assert!(
            matches!(view, OverlayView::Live),
            "back_to_live must still reset the view once the row count is captured"
        );
    }

    // -- handle_share_screenshot (the `OverlayApp::ui` sequencing itself) ---

    /// `handle_share_screenshot` is the actual fix site for issue #82's
    /// async round-trip regression (see `take_pending_screenshot_bound`'s
    /// doc comment): it is the code that runs inside `OverlayApp::ui`, not
    /// just the pure helpers it calls. Driving `OverlayApp::ui` itself is
    /// infeasible — `eframe::Frame` has only `pub(crate)` fields and no
    /// public constructor — but this sequencing never actually touches
    /// `_frame`, so it was pulled out into its own function that needs
    /// only `&egui::Context` and `&mut Option<f32>`, both of which a bare
    /// `egui::Context::default()` and a plain local variable stand in for
    /// here, mirroring `handle_screenshot_events_routes_a_screenshot_
    /// event_to_the_writer`'s use of a synthetic `Event::Screenshot` with
    /// no live window or clipboard.
    ///
    /// A frame with no screenshot event must leave the pending bound
    /// untouched and must never call `write` — otherwise every idle frame
    /// between the Share click and the async reply would either discard
    /// the stashed bound (issue #82) or overwrite the clipboard with
    /// nothing to write.
    #[test]
    fn handle_share_screenshot_leaves_the_bound_untouched_on_a_frame_with_no_event() {
        let ctx = egui::Context::default();
        let mut pending_screenshot_bound = Some(123.0);
        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();

        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            handle_share_screenshot(&ctx, &mut pending_screenshot_bound, |image| {
                written.push(image);
            });
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            pending_screenshot_bound,
            Some(123.0),
            "a frame without a screenshot event must not consume the pending bound"
        );
        assert!(
            written.is_empty(),
            "write must not run without a Screenshot event"
        );
    }

    /// The frame that actually carries the async `Event::Screenshot` reply
    /// must consume (reset to `None`) the pending bound and hand exactly
    /// one cropped image to `write` — proving the whole sequencing inside
    /// `OverlayApp::ui` (collect → decide the bound → crop → write), not
    /// just its pure helpers in isolation.
    #[test]
    fn handle_share_screenshot_consumes_the_bound_and_writes_on_the_frame_the_event_lands() {
        let ctx = egui::Context::default();
        let mut pending_screenshot_bound = Some(6.0);
        let image = std::sync::Arc::new(egui::ColorImage::filled(
            [4, 10],
            egui::Color32::from_rgb(1, 2, 3),
        ));
        let input = egui::RawInput {
            events: vec![egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                user_data: egui::UserData::default(),
                image: image.clone(),
            }],
            ..Default::default()
        };

        let mut written: Vec<std::sync::Arc<egui::ColorImage>> = Vec::new();
        let output = ctx.run_ui(input, |_ui| {
            handle_share_screenshot(&ctx, &mut pending_screenshot_bound, |image| {
                written.push(image);
            });
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            pending_screenshot_bound, None,
            "a landed event must consume (reset) the pending bound"
        );
        assert_eq!(
            written.len(),
            1,
            "the landed screenshot must be cropped and written exactly once"
        );
        // pending bound of 6.0 at pixels_per_point 1.0 (the context's
        // default) crops the 10-tall source image down to 6 rows.
        assert_eq!(written[0].size, [4, 6]);
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

    /// Issue #185: the title row reserves the toggle pill's width on top of
    /// the chevron's, so the boss name never runs under either control —
    /// while the subtitle row, which has no pill, still reserves the chevron
    /// strip alone.
    #[test]
    fn the_title_row_reserves_the_toggle_pill_as_well_as_the_chevron() {
        for width in [MIN_INNER_SIZE.x, default_inner_width(), 1_200.0] {
            let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(width, 20.0));
            assert_eq!(
                title_text_rect(row).right(),
                row.right() - TITLE_RIGHT_CONTROLS_WIDTH
            );
            assert_eq!(
                header_text_rect(row).right(),
                row.right() - HEADER_RIGHT_CONTROL_WIDTH
            );
            assert!(
                title_text_rect(row).right() < header_text_rect(row).right(),
                "the title must stop short of the subtitle at {width}pt wide"
            );
        }
    }

    /// Issue #185: the title row's toggle pill sits immediately left of the
    /// chevron's reserved strip, at its full width, exactly inside the strip
    /// `title_text_rect` keeps clear for it — and the click-through button's
    /// published hit box is the pill's *first* slot, so the box that
    /// `WM_NCHITTEST` carves out really is the button the user sees.
    #[test]
    fn the_title_toggle_pill_sits_left_of_the_chevron() {
        let row = egui::Rect::from_min_size(egui::pos2(7.0, 3.0), egui::vec2(400.0, 22.0));
        let pill = title_toggle_pill_rect(row, 18.0);

        assert_eq!(
            pill.right(),
            row.right() - HEADER_RIGHT_CONTROL_WIDTH - TITLE_TOGGLE_GAP_X,
            "the pill's right edge is the chevron strip's left edge, minus the gap"
        );
        assert!(
            pill.right() < chevron_rect(row).left(),
            "the pill must sit clear of the chevron's own box"
        );
        assert_eq!(pill.width(), TITLE_TOGGLE_PILL_WIDTH);
        assert_eq!(pill.height(), 18.0);
        assert_eq!(pill.center().y, row.center().y);
        assert!(
            pill.left() >= title_text_rect(row).right(),
            "the pill must not overlap the title text"
        );

        let button = click_through_button_slot(pill);
        assert_eq!(button.left(), pill.left() + TOGGLE_PAD_X);
        assert_eq!(button.width(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(button.center().y, pill.center().y);
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

    /// Issue #183: a capture of the transparent overlay is flattened to
    /// fully opaque before the clipboard write — the premultiplied RGB is
    /// left exactly as it is, only the alpha byte changes — while an
    /// already-opaque capture comes back as the very same `Arc`, with no
    /// second pixel buffer allocated.
    #[test]
    fn flatten_screenshot_alpha_forces_opacity_only_when_it_has_to() {
        let translucent = std::sync::Arc::new(egui::ColorImage::new(
            [2, 1],
            vec![
                egui::Color32::from_rgba_premultiplied(10, 20, 30, 40),
                egui::Color32::from_rgba_premultiplied(1, 2, 3, 255),
            ],
        ));
        let flattened = flatten_screenshot_alpha(&translucent);
        assert!(
            flattened.pixels.iter().all(|pixel| pixel.a() == u8::MAX),
            "every pixel must be opaque: {:?}",
            flattened.pixels
        );
        assert_eq!(
            (
                flattened.pixels[0].r(),
                flattened.pixels[0].g(),
                flattened.pixels[0].b()
            ),
            (10, 20, 30),
            "the premultiplied colour channels must be left untouched"
        );

        let opaque = std::sync::Arc::new(egui::ColorImage::new(
            [1, 1],
            vec![egui::Color32::from_rgba_premultiplied(1, 2, 3, 255)],
        ));
        assert!(
            std::sync::Arc::ptr_eq(&opaque, &flatten_screenshot_alpha(&opaque)),
            "an opaque capture must not be copied"
        );
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

    /// Issue #80.2: `fmt_pct0` is `fmt_share` minus the decimal place —
    /// same rounding, same trailing `%`, just `{:.0}` instead of `{:.1}`.
    #[test]
    fn fmt_pct0_drops_the_decimal_place() {
        let cases = [(0.0, "0%"), (12.34, "12%"), (12.6, "13%"), (100.0, "100%")];
        for (input, expected) in cases {
            assert_eq!(fmt_pct0(input), expected, "fmt_pct0({input})");
        }
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

    /// Branch-before-rounding regression: the 1- and 2-decimal bands
    /// truncate rather than round (`fmt_short`'s doc comment), so they can
    /// never round themselves over a band boundary — but the *coarsest*
    /// band's own threshold is still decided by rounding the scaled value
    /// to the nearest whole number, since a scaled value close enough to
    /// the next full order of magnitude (`[999.5, 1000)`, e.g. `999.95`)
    /// should escalate straight to the 0-decimal band (`"1000K"`) instead
    /// of printing a falsely-precise truncated `"999.9K"`. `999_499` sits
    /// one below that threshold and stays in the 1-decimal band. The Dps
    /// column budgets 5 chars for the digit run alone, excluding the
    /// trailing K/M/B suffix and any leading `-`.
    #[test]
    fn fmt_short_rounds_before_choosing_the_decimal_branch() {
        let cases: [(i64, &str); 6] = [
            (999_499, "999.4K"),
            (999_500, "1000K"),
            (999_499_000, "999.4M"),
            (999_500_000, "1000M"),
            (99_999, "99.99K"),
            (99_999_000, "99.99M"),
        ];
        for (input, expected) in cases {
            let out = fmt_short(input);
            assert_eq!(out, expected, "fmt_short({input})");
            let digits = out
                .trim_start_matches('-')
                .trim_end_matches(['K', 'M', 'B']);
            assert!(
                digits.len() <= 5,
                "fmt_short({input}) = {out:?} exceeds the 5-char pre-suffix digit budget"
            );
        }
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

    // -- live boss now wins over the remembered one (issue #131) -----------

    #[test]
    fn title_prefers_the_live_boss_over_a_remembered_scene_boss() {
        // Issue #131's precedence inversion: a genuine currently-engaged
        // boss (`is_boss: true`) now outranks `scene_boss_name`, the
        // opposite of issue #125's original "final boss only, always" rule.
        // This is what makes the raid case below correct — see the raid
        // test and `encounter_title`'s doc comment for why.
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Rathalos"),
            is_boss: true,
            scene_boss_name: Some("Blazing Mech 05"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Rathalos");
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
    fn title_prefers_scene_boss_name_before_any_hit_lands() {
        // The case the function's doc comment leads with: the dungeon's
        // final boss is already known from a prior pull, but this
        // encounter hasn't hit anything yet — `boss_monster_id` is `None`.
        // The scene boss name must still win, via the unconditional `if
        // let` ahead of the `match e.boss_monster_id`, not "No target".
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: Some("Blazing Mech 05"),
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Blazing Mech 05");
    }

    #[test]
    fn title_falls_through_to_existing_rules_when_scene_boss_name_absent() {
        // No dungeon final boss has been learned yet (e.g. a scene's first
        // run this session, or a non-dungeon scene) — the pre-issue-#125
        // precedence still applies unchanged.
        let e = EncounterInfo {
            boss_monster_id: Some(103),
            boss_name: Some("Rathalos"),
            is_boss: true,
            scene_boss_name: None,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Rathalos");
    }

    // -- multi-boss scenes ask for a selection (issue #150) ----------------

    #[test]
    fn title_asks_for_a_boss_selection_in_a_multi_boss_scene_with_nothing_engaged() {
        // Issue #150: in a raid that offers three separately selectable
        // bosses, `scene_boss_name` is deliberately suppressed (naming one
        // is a guess that is wrong two times out of three). "No target" is
        // the wrong caption for that: there is a target, the party just
        // hasn't picked it yet.
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: None,
            multi_boss_scene: true,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "Select a boss");
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

    #[test]
    fn title_keeps_no_target_in_an_ordinary_scene_with_nothing_engaged() {
        // The regression guard: an ordinary dungeon (or town) with nothing
        // engaged and nothing remembered is unchanged by issue #150.
        let e = EncounterInfo {
            boss_monster_id: None,
            scene_boss_name: None,
            multi_boss_scene: false,
            ..Default::default()
        };
        assert_eq!(encounter_title(&e), "No target");
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
            imagines: [None, None],
            imagine_tiers: [None, None],
            skills: Vec::new(),
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

    // -- name_suffix (issue #168): AbilityScore/SeasonStrength inline with
    // the name instead of their own stat column ---------------------------

    fn sample_score_row(ability_score: Option<u32>, season_strength: Option<u32>) -> PlayerRow {
        PlayerRow {
            season_strength,
            ..sample_row(ability_score)
        }
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

    /// egui stacks title + subtitle + stat row, so the band pays two gaps,
    /// not one, and they are not the same size: title -> subtitle pays
    /// `ITEM_SPACING_Y`, subtitle -> stat row pays `HEADER_STAT_ROW_GAP`.
    /// All three rows are unconditional (issue #91), so this is the band's
    /// only case — 68pt at the real button-row height.
    #[test]
    fn header_band_height_covers_both_text_rows_both_gaps_and_the_button_row() {
        let button_row_height = 18.0;
        let expected = TITLE_LINE_HEIGHT
            + ITEM_SPACING_Y
            + SUBTITLE_LINE_HEIGHT
            + HEADER_STAT_ROW_GAP
            + button_row_height;
        assert_eq!(header_band_height(button_row_height), expected);
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
    }

    /// Issue #91 regression. The header must be a fixed-height band whether
    /// or not an area name is known: the app's idle "No target" state used
    /// to skip the subtitle row *and* the gap above it, collapsing the band
    /// from 68 to 50 and lifting the entire stat-pill row — timer, DPS,
    /// damage, toggles — 18pt (`SUBTITLE_LINE_HEIGHT + ITEM_SPACING_Y`) up
    /// the window the moment the boss/area data arrived or went away. A
    /// pixel scan of the live window measured the band bottom at y=51
    /// without an area name against y=69 with one.
    ///
    /// Asserted twice over: on the layout helpers `draw_header` lays the
    /// band out with, and on where the stat row's chrome actually lands
    /// when the same snapshot is rendered with and without a scene name.
    #[test]
    fn a_missing_area_name_does_not_collapse_the_header_or_lift_the_stat_row() {
        // The band, and the stat row's offset into it, are single numbers —
        // there is no longer a subtitle-present/absent pair to diverge.
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
        let stat_row_top = header_text_band_height() + HEADER_STAT_ROW_GAP;
        assert_eq!(stat_row_top, 46.0);

        // Painted truth: `header_test_snapshot` has no scene at all, so it
        // is exactly the idle state that used to collapse the band.
        let without = header_test_snapshot(30_100_000_000);
        assert!(encounter_subtitle(&without.encounter).is_none());
        let mut with = header_test_snapshot(30_100_000_000);
        with.encounter.scene_name = Some("Frozen Bahaar's Sanctum");
        assert!(encounter_subtitle(&with.encounter).is_some());

        // The stat-pill chrome is the row's own ink, and it must not move
        // by so much as a point between the two. `PILL_FILL` picks the
        // first *value* pill (the timer wears `TIMER_PILL_FILL`, a colour
        // of its own), which is the same shape in both renders.
        let pill_without = header_painted_boxes(&without).fill_box(PILL_FILL);
        let pill_with = header_painted_boxes(&with).fill_box(PILL_FILL);
        assert!(
            (pill_with.top() - pill_without.top()).abs() < 0.01,
            "the stat-pill row sits at y={} with an area name but y={} without \
             it — a {}pt jump",
            pill_with.top(),
            pill_without.top(),
            pill_with.top() - pill_without.top()
        );
        assert!(
            (pill_with.height() - pill_without.height()).abs() < 0.01,
            "the stat-pill row is {}pt tall with an area name and {}pt without",
            pill_with.height(),
            pill_without.height()
        );
    }

    /// Issue #91's header grid: `TITLE_LINE_HEIGHT + ITEM_SPACING_Y +
    /// SUBTITLE_LINE_HEIGHT == 40.0`, grown from the source's `Height="36"`
    /// (`20 + 2 + 14`) so the boss name and area name each get the vertical
    /// room the reference render gives them. Pinned as a sum, not three
    /// separate literals, so a future edit to any one constant can't drift
    /// without this test catching it.
    #[test]
    fn the_title_and_subtitle_lines_add_up_to_the_source_header_grid() {
        let total = TITLE_LINE_HEIGHT + ITEM_SPACING_Y + SUBTITLE_LINE_HEIGHT;
        assert_eq!(total, 40.0);
        // …and the text band is that grid, whole, always (issue #91).
        assert_eq!(header_text_band_height(), total);
    }

    /// The whole point of issue #91's `HEADER_STAT_ROW_GAP`: the stat row
    /// must clear the subtitle's descenders by more than the 2pt every other
    /// adjacent pair gets, and the band must budget that extra room — 68pt
    /// with a subtitle, against the 60 it used to be.
    #[test]
    fn the_stat_row_gap_is_wider_than_the_ordinary_row_spacing() {
        const { assert!(HEADER_STAT_ROW_GAP > ITEM_SPACING_Y) };
        assert_eq!(header_band_height(BUTTON_ROW_HEIGHT), 68.0);
    }

    /// The emblem is bled off the title row's left edge and overhangs the
    /// text band it decorates in *both* directions. Only the left bleed and
    /// the top overhang are ever cut (by the panel's own edges); the bottom
    /// overhang is painted in full, since `draw_header` clips the mark to
    /// the whole header band and the stat row is inset clear of it. Pinning
    /// the bottom overhang here is what keeps that so: shrink the box back
    /// inside the text band and the mark stops being the thing the reference
    /// render shows.
    #[test]
    fn the_header_emblem_bleeds_off_the_left_edge_and_out_of_the_text_band() {
        let row = egui::Rect::from_min_size(egui::pos2(10.0, 40.0), egui::vec2(380.0, 20.0));
        let band = header_text_band_height();
        let rect = header_emblem_rect(row, band);

        assert!(rect.left() < row.left());
        // …and by exactly enough that the visible remainder is the gutter.
        assert_eq!(rect.right() - row.left(), HEADER_GUTTER_WIDTH);

        assert!(rect.top() < row.top());
        assert!(rect.bottom() > row.top() + band);
        assert_eq!(rect.width(), HEADER_EMBLEM_SIZE);
        assert_eq!(rect.height(), HEADER_EMBLEM_SIZE);
    }

    /// The source's centering, reproduced exactly for the case it was read
    /// off (`Height="36"` grid, `Margin="… -8"`): a top edge 8pt above the
    /// grid, i.e. the box hangs `HEADER_EMBLEM_BOTTOM_BLEED` further below
    /// the band than above it.
    ///
    /// That asymmetry is the design, not an oversight — issue #91 squared it
    /// up on the assumption it was a bug and had to be reverted, so the
    /// numbers are pinned here. Against issue #91's 40pt text band the
    /// formula centres 60pt on `40 + 8 = 48`: top 6pt above the band (cut by
    /// the panel's own top edge) and bottom at `row.top() + 54`, 14pt below
    /// it — painted in full, down inside the stat row's height, which the
    /// row's own inset keeps clear of. See `header_emblem_rect`.
    #[test]
    fn the_header_emblem_hangs_further_below_the_text_band_than_above_it() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(380.0, 20.0));
        // The source's own 36pt grid — the case the `-8` margin was read off.
        assert_eq!(header_emblem_rect(row, 36.0).top(), -8.0);

        let band = header_text_band_height();
        let rect = header_emblem_rect(row, band);
        assert_eq!(rect.top(), row.top() - 6.0);
        assert_eq!(rect.bottom(), row.top() + 54.0);

        let above = row.top() - rect.top();
        let below = rect.bottom() - (row.top() + band);
        assert!(
            (below - (above + HEADER_EMBLEM_BOTTOM_BLEED)).abs() < 0.01,
            "the negative bottom margin should hang the emblem \
             {HEADER_EMBLEM_BOTTOM_BLEED}pt further below the band than above it: \
             above {above}, below {below}"
        );
    }

    /// The relation the emblem box's horizontal geometry rests on, pinned as
    /// constants so neither can be changed alone: shrinking or growing the
    /// mark without moving its left bleed would drag the visible gutter —
    /// and with it the title/subtitle indent — off `HEADER_GUTTER_WIDTH`.
    #[test]
    fn the_emblem_box_preserves_the_gutter() {
        const { assert!(HEADER_EMBLEM_LEFT_BLEED + HEADER_EMBLEM_SIZE == HEADER_GUTTER_WIDTH) };
        assert_eq!(HEADER_EMBLEM_LEFT_BLEED, -26.0);
        assert_eq!(HEADER_EMBLEM_SIZE, 60.0);
        // And the mark is deliberately *not* the band: it is shorter than
        // the drag band it is clipped to — so the clip never cuts its
        // bottom — and taller than the text band it is centered on, which is
        // what makes it overhang those rows at all.
        assert!(HEADER_EMBLEM_SIZE < header_band_height(BUTTON_ROW_HEIGHT));
        assert!(HEADER_EMBLEM_SIZE > header_text_band_height());
    }

    /// The band the drag surface covers is the text rows plus
    /// `HEADER_STAT_ROW_GAP` plus the stat-pill row, so the text band is
    /// strictly shorter than it. Both numbers exist and neither may be
    /// substituted for the other: the drag surface, the wash and the gutter
    /// emblem's clip are the band's, only the title/subtitle rows and the
    /// emblem's *centering* are the text band's.
    #[test]
    fn the_text_band_is_the_drag_band_minus_the_stat_pill_row() {
        let text = header_text_band_height();
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        assert_eq!(band, text + HEADER_STAT_ROW_GAP + BUTTON_ROW_HEIGHT);
        assert!(text < band);
        assert_eq!(text, 40.0);
    }

    // -- header background wash (issue #59, #62, #81) --------------------

    /// A stand-in central-panel rect for the wash geometry tests — wider and
    /// far taller than the wash itself, like the real panel.
    fn wash_test_panel() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(12.0, 30.0), egui::vec2(400.0, 300.0))
    }

    /// A stand-in wash height for the geometry tests below — issue #81 made
    /// this the caller's to choose (`draw_header` derives it from
    /// `header_text_band_height`) rather than a fixed constant, so these
    /// tests exercise the geometry with an arbitrary value of their own.
    const WASH_TEST_HEIGHT: f32 = 34.0;

    /// The wash is inset from the panel on both sides and its top so its
    /// square corners stay inside the panel's rounded border, and it runs
    /// down exactly the `height` it is given rather than to the panel's —
    /// it is decoration behind the header rows, not a full-height
    /// background.
    #[test]
    fn the_header_wash_is_inset_from_the_panel_and_keeps_its_given_height() {
        let panel = wash_test_panel();
        let wash = header_wash_rect(panel, WASH_TEST_HEIGHT);
        assert_eq!(wash.left() - panel.left(), HEADER_WASH_INSET);
        assert_eq!(panel.right() - wash.right(), HEADER_WASH_INSET);
        assert_eq!(wash.top() - panel.top(), HEADER_WASH_INSET);
        assert_eq!(wash.width(), panel.width() - 2.0 * HEADER_WASH_INSET);
        assert_eq!(wash.height(), WASH_TEST_HEIGHT);
        assert!(wash.bottom() < panel.bottom());
    }

    /// The wash emblem hangs off the wash's right edge by exactly the
    /// source's `-25` right margin, and is vertically centered on the wash —
    /// the mirror of the gutter emblem's left-edge bleed, and the reason the
    /// wash must be painted through its own clip rect.
    #[test]
    fn the_wash_emblem_bleeds_off_the_right_edge_by_the_named_overhang() {
        let wash = header_wash_rect(wash_test_panel(), WASH_TEST_HEIGHT);
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
        let wash = header_wash_rect(wash_test_panel(), WASH_TEST_HEIGHT);
        let emblem = header_wash_emblem_rect(wash);
        assert!(emblem.height() > wash.height());
        assert!(emblem.top() < wash.top());
        assert!(emblem.bottom() > wash.bottom());
    }

    /// Issue #91 inverts issue #81's rule. The wash is the header band's
    /// *background*, so it must run behind the stat-pill row as well as the
    /// text rows. Issue #158 corrects where it must stop: not
    /// `header_band_height` itself (that left an 8pt gap of bare panel fill,
    /// with the separator's faint line inside it, between the wash and the
    /// first row) but `first_player_row_top_offset` — the band plus the
    /// `ui.separator()` and the layout gap before it, which is where the
    /// first player row genuinely starts. What it must still never do is
    /// bleed *past* that row, which is exactly where the old fixed `98.0`pt
    /// wash went wrong.
    #[test]
    fn wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row() {
        let panel = wash_test_panel();
        let button_row_height = 18.0;
        let text_band = header_text_band_height();
        let band = header_band_height(button_row_height);
        let wash = header_wash_rect(panel, first_player_row_top_offset(band) - HEADER_WASH_INSET);
        let stat_pill_row_top = panel.top() + text_band + HEADER_STAT_ROW_GAP;
        let stat_pill_row_bottom = stat_pill_row_top + button_row_height;
        let first_player_row_top = panel.top() + first_player_row_top_offset(band);

        assert!(
            wash.top() < stat_pill_row_top,
            "wash top {} starts below the stat-pill row at {stat_pill_row_top}",
            wash.top()
        );
        assert!(
            wash.bottom() >= stat_pill_row_bottom,
            "wash bottom {} stops short of the stat-pill row's bottom at \
             {stat_pill_row_bottom}",
            wash.bottom()
        );
        assert!(
            wash.bottom() <= first_player_row_top,
            "wash bottom {} bleeds past the first player row at \
             {first_player_row_top}",
            wash.bottom()
        );
        // Flush, not merely inside: the only slack is the inset the wash
        // is pushed down from the panel's top edge by.
        assert_eq!(wash.bottom(), first_player_row_top);
    }

    /// Issue #91: the wash's *gradient* — not just its clip — has to span
    /// the whole band, measured on what `draw_header` actually paints. A
    /// change that shrank the quad back to the text band while leaving the
    /// emblem alone would pass every pure-geometry test above and still be
    /// the bug.
    ///
    /// Issue #158: asserting the gradient's *height* against `band -
    /// HEADER_WASH_INSET` is exactly what let the 8pt-short bug hide —
    /// that assertion was true both before and after the fix, since it
    /// never checks where the first player row actually starts. Asserting
    /// the gradient's *bottom* against `first_player_row_top_offset`
    /// instead (derived from `gradient.top()`, so this doesn't also have
    /// to assume where the panel's own top edge landed) ties the two
    /// together so they cannot drift apart again.
    #[test]
    fn the_wash_gradient_spans_the_whole_header_band() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let gradient = frame.gradient_box();

        let band = header_band_height(BUTTON_ROW_HEIGHT);
        // `gradient.top()` is `panel.top() + HEADER_WASH_INSET` (see
        // `header_wash_rect`), so subtracting the inset back out recovers
        // the panel's own top edge without this test having to assume a
        // fixed value for it.
        let panel_top = gradient.top() - HEADER_WASH_INSET;
        let first_player_row_top = panel_top + first_player_row_top_offset(band);
        assert!(
            (gradient.bottom() - first_player_row_top).abs() < 0.01,
            "the wash gradient's bottom is {}, not the first player row's \
             top at {first_player_row_top} — a wash that stops at the bare \
             header band leaves a gap of bare panel fill above the rows",
            gradient.bottom()
        );

        // …and it really is behind the stat row's ink, not merely tall.
        let stat_ink = frame.glyph_boxes(GlyphIcon::Timer)[0]
            .union(frame.text_box(&fmt_duration(snapshot.duration_ms)));
        assert!(
            gradient.bottom() >= stat_ink.bottom(),
            "the wash gradient ends at {} — above the timer's ink {stat_ink:?}",
            gradient.bottom()
        );
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
            ability_score: Some(99_999),
            season_strength: Some(9_999),
            imagines: [Some(99_999), Some(99_999)],
            imagine_tiers: [Some(IMAGINE_MAX_TIER), Some(IMAGINE_MAX_TIER)],
            skills: Vec::new(),
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

    // -- icon slot geometry (issue #9, issue #33) --------------------------

    fn row_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(300.0, ROW_HEIGHT))
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

    /// Runtime reads, not const-vs-const compares (clippy's
    /// `assertions_on_constants`) — same trick
    /// `font_scale_matches_the_source_metrics` uses.
    #[test]
    fn chrome_border_is_translucent_but_the_fill_is_not() {
        assert_eq!(PANEL_BORDER_COLOR.a(), 128);
        // Issue #182: no baked-in baseline left to multiply against, so the
        // slider owns the panel's transparency outright.
        assert_eq!(PANEL_FILL.a(), 255);
    }

    /// Issue #182: the slider's endpoints have to mean what they say. A
    /// full-range drag must land on a completely solid and a completely
    /// absent backdrop, with nothing baked in to cap either end.
    #[test]
    fn panel_opacity_endpoints_are_solid_and_gone() {
        assert_eq!(
            PANEL_FILL.gamma_multiply(Settings::OPACITY_MAX).a(),
            255,
            "100% must paint a fully opaque panel"
        );
        assert_eq!(
            PANEL_FILL.gamma_multiply(Settings::OPACITY_MIN).a(),
            0,
            "0% must paint no panel at all"
        );
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
        let total: f32 = SKILL_COLUMN_ORDER.iter().map(|c| c.width()).sum();
        assert!(
            total <= SKILL_WINDOW_SIZE.x - 2.0 * SKILL_HEADER_PAD_X,
            "columns total {total}"
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
        let fill = skill_selected_tab_rect(tabs, 26.0);
        assert_eq!(fill.left(), tabs.left());
        assert_eq!(fill.top(), tabs.top());
        assert_eq!(fill.height(), tabs.height());
        assert_eq!(fill.width(), 26.0 + 2.0 * SKILL_HEADER_PAD_X);
        assert!(
            fill.right() < tabs.right(),
            "the rest of the strip must stay window fill"
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
        let col_header_rect = skill_column_header_rect(rect, tabs_rect);
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

    // -- breakdown-window chrome gestures (issue #218) ----------------------

    /// The window rect these gesture tests measure against — off-origin so
    /// an accidental `0.0` in the maths cannot pass by coincidence.
    fn skill_window_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(120.0, 80.0), egui::vec2(880.0, 520.0))
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
        let rows = skill_rows_rect(rect, skill_column_header_rect(rect, tabs));
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

    /// The deaths pill reserves its room off the close button's *hit* size,
    /// so growing that button (issue #218) pushes the pill left instead of
    /// letting the two overlap.
    #[test]
    fn deaths_pill_clears_the_close_button_by_one_header_pad() {
        let rect = skill_window_rect();
        let header = skill_header_rect(rect);
        let pill_width = 64.0;
        let left = skill_deaths_pill_left(header, pill_width);
        assert_eq!(
            left + pill_width + SKILL_HEADER_PAD_X,
            skill_close_rect(rect).left()
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
            ..sample_row(None)
        };
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_MIN_SIZE);
        let mut sort = skills::SkillSort::default();

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
                            &mut sort,
                            SkillWindowSource::Live,
                            &icons,
                            1.0,
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

    /// Every `Shape::Rect` a frame painted, flattened out of the `Vec`
    /// nesting -- `collect_row_boxes` deliberately keeps only text and
    /// meshes, and a scrollbar is neither.
    fn painted_rects(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) => out.push(rect.rect),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    painted_rects(s, out);
                }
            }
            _ => {}
        }
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
        let mut sort = skills::SkillSort::default();
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
                        &mut sort,
                        SkillWindowSource::Live,
                        &icons,
                        1.0,
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
                    &mut sort,
                    SkillWindowSource::Live,
                    &icons,
                    1.0,
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
        let rows = skill_rows_rect(screen_rect, skill_column_header_rect(screen_rect, tabs));
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

    /// The panel is deliberately *not* the source's slate `#232830` — that
    /// reads as washed-out grey over game footage. Lock the near-black.
    #[test]
    fn panel_fill_is_near_black_not_slate() {
        // Compare through the same constructor: `Color32` stores premultiplied
        // channels, so `to_tuple()` would not round-trip the (18, 18, 22).
        assert_eq!(PANEL_FILL, egui::Color32::from_rgb(18, 18, 22));
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
    // `class_icon_file`; this one checks the same property from the
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

    #[test]
    fn default_inner_height_matches_title_plus_header_plus_separator_plus_rows_plus_gaps() {
        let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        // Decision 3: every gap above the row list is inside the band bar
        // one — stat row->separator, the layout's ordinary
        // `ITEM_SPACING_Y`. `draw_rows` zeroes `item_spacing.y` for its own
        // scope, so there is no gap before the first row or between rows.
        // The band is spelled out here (rather than reusing
        // `header_band_height`, which is what the function itself calls) so
        // this stays an independent statement of the sum.
        let band = TITLE_LINE_HEIGHT
            + ITEM_SPACING_Y
            + SUBTITLE_LINE_HEIGHT
            + HEADER_STAT_ROW_GAP
            + BUTTON_ROW_HEIGHT;
        let expected = band + SEPARATOR_HEIGHT + rows + ITEM_SPACING_Y;
        assert_eq!(default_inner_height(), expected);
        // Issue #91 grew this from `652.0` -> `658.0` (a 2pt taller title
        // line, and `HEADER_STAT_ROW_GAP` above the stat row in place of
        // `ITEM_SPACING_Y`) -> `676.0` here: the band now reserves the
        // subtitle's line and gap whether or not an area name is known, so
        // the default window has to budget them too or it opens 18pt short
        // of the 20 rows it promises.
        assert_eq!(default_inner_height(), 676.0);
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

    #[test]
    fn default_size_stays_above_the_min_inner_size() {
        // `with_min_inner_size` is [220.0, 90.0] (unaffected by issue #26);
        // the default opening size must never start below its own floor.
        assert!(default_inner_width() >= 220.0);
        assert!(default_inner_height() >= 90.0);
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

    #[test]
    fn reset_to_defaults_inner_height_matches_header_plus_separator_plus_five_rows_plus_gap() {
        let rows = RESET_TO_DEFAULTS_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        let expected =
            header_band_height(BUTTON_ROW_HEIGHT) + SEPARATOR_HEIGHT + rows + ITEM_SPACING_Y;
        assert_eq!(reset_to_defaults_inner_height(), expected);
    }

    #[test]
    fn reset_to_defaults_inner_height_is_shorter_than_the_twenty_row_default_by_exactly_the_row_delta()
     {
        // Both heights derive from the same `inner_height_for_rows` formula
        // now, so the *only* thing that can differ between them is the row
        // count. Asserting the exact gap (not just `<`) is what actually
        // catches drift: if a future edit changes the top-level formula for
        // one caller but not the other, this fails even though both heights
        // still individually "fit their rows".
        let row_delta = (DEFAULT_VISIBLE_ROWS - RESET_TO_DEFAULTS_VISIBLE_ROWS) as f32;
        assert_eq!(
            default_inner_height() - reset_to_defaults_inner_height(),
            row_delta * ROW_HEIGHT
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

    /// Every row shares the same damage, so `row_bar_frac` (relative to the
    /// top row) is exactly `1.0` for all of them and `share_bar_paints`'
    /// `fill_rect` therefore equals the *full* row rect for every row — the
    /// only painted shape wide enough to use as each row's ground-truth
    /// rect in `RowFrame::row_rects`.
    fn rows_test_snapshot(n: usize) -> Snapshot {
        Snapshot {
            duration_ms: 90_000,
            total_damage: 1_000 * n as i64,
            total_dps: 12_345.0,
            rows: (0..n)
                .map(|i| PlayerRow {
                    name: format!("P{i}"),
                    damage: 1_000,
                    ..sample_row(None)
                })
                .collect(),
            encounter: EncounterInfo::default(),
        }
    }

    /// One `draw_rows` frame's painted text and mesh geometry — mirrors
    /// `HeaderFrame`/`collect_painted_boxes` (issue #75) but for the row
    /// list (issue #83's regression harness): meshes rather than filled
    /// rects, because `draw_row` paints the share bar and hover highlight
    /// as gradient meshes (`Shape::Mesh`), never a flat `Shape::Rect`.
    struct RowFrame {
        /// In paint order, so a test can compare two texts' indices to pin
        /// z-order (egui paints shapes in call order) — that is how the
        /// inline name suffix is proven to paint *before* the stat-column
        /// loop. The `Color32` is the galley's own text color, which is
        /// what makes the suffix's dimming (`NAME_SUFFIX_ALPHA`) checkable
        /// from the painted output rather than from the call site.
        texts: Vec<(String, egui::Rect, egui::Color32)>,
        meshes: Vec<egui::Rect>,
    }

    impl RowFrame {
        /// The union of every text shape painted for `value` (a player
        /// name here).
        fn text_box(&self, value: &str) -> egui::Rect {
            self.texts
                .iter()
                .filter(|(painted, ..)| painted == value)
                .map(|(_, rect, _)| *rect)
                .reduce(egui::Rect::union)
                .unwrap_or_else(|| panic!("draw_rows never painted {value:?}: {:?}", self.texts))
        }

        /// The index in paint order of the first text shape painted for
        /// `value`, plus the color it was painted in.
        fn text_paint(&self, value: &str) -> (usize, egui::Color32) {
            self.texts
                .iter()
                .enumerate()
                .find(|(_, (painted, ..))| painted == value)
                .map(|(i, (_, _, color))| (i, *color))
                .unwrap_or_else(|| panic!("draw_rows never painted {value:?}: {:?}", self.texts))
        }

        /// Every row's own rect, identified by `rows_test_snapshot`'s
        /// equal-damage trick: nothing else `draw_row` paints is
        /// `ROW_HEIGHT` tall — the accent line is
        /// `SHARE_BAR_ACCENT_THICKNESS`, the hover highlight only paints
        /// while hovered (never true in a static render), the class icon is
        /// `ICON_SIZE`, and the two Imagine slots (issue #33) are
        /// `IMAGINE_SIZE` — a filled slot's `Shape::Mesh` and an empty
        /// slot's `Shape::Circle` are both filtered out the same way.
        /// Sorted top-to-bottom so index `i` here really is row `i`.
        fn row_rects(&self) -> Vec<egui::Rect> {
            let mut rects: Vec<egui::Rect> = self
                .meshes
                .iter()
                .copied()
                .filter(|r| (r.height() - ROW_HEIGHT).abs() < 0.01)
                .collect();
            rects.sort_by(|a, b| a.top().partial_cmp(&b.top()).unwrap());
            rects
        }
    }

    fn collect_row_boxes(shape: &egui::Shape, clip: egui::Rect, frame: &mut RowFrame) {
        match shape {
            egui::Shape::Text(text) => frame.texts.push((
                text.galley.text().to_string(),
                egui::Rect::from_min_size(text.pos, text.galley.size()).intersect(clip),
                text.override_text_color
                    .or_else(|| {
                        text.galley
                            .job
                            .sections
                            .first()
                            .map(|section| section.format.color)
                    })
                    .filter(|color| *color != egui::Color32::PLACEHOLDER)
                    .unwrap_or(text.fallback_color),
            )),
            egui::Shape::Mesh(mesh) => frame.meshes.push(mesh.calc_bounds().intersect(clip)),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_row_boxes(s, clip, frame);
                }
            }
            _ => {}
        }
    }

    /// Renders `draw_rows` in a `screen_rect` of `width` x `height` and
    /// hands back everything it painted — same pattern as
    /// `header_painted_boxes` (issue #75), generalized to a caller-chosen
    /// size so both the default-size regression harness (issue #83) and
    /// the overflow/scroll tests below (issue #84) can reuse it. Measures
    /// *inside* the `ScrollArea`'s own viewport clip, since
    /// `collect_row_boxes` intersects every shape with the clip rect it was
    /// actually painted under — exactly like `collect_painted_boxes` does.
    fn rows_painted_boxes(snapshot: &Snapshot, width: f32, height: f32) -> RowFrame {
        rows_painted_boxes_with(snapshot, &Settings::default(), width, height)
    }

    /// `rows_painted_boxes` with the settings under test spelled out —
    /// needed by anything that has to render a non-default column set
    /// (issue #168's inline `AbilityScore`/`SeasonStrength`, which
    /// `Settings::default` does not enable).
    fn rows_painted_boxes_with(
        snapshot: &Snapshot,
        settings: &Settings,
        width: f32,
        height: f32,
    ) -> RowFrame {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            ..Default::default()
        };

        let mut frame = RowFrame {
            texts: Vec::new(),
            meshes: Vec::new(),
        };
        let output = ctx.run_ui(input, |ui| {
            draw_rows(ui, snapshot, settings, &icons, &mut None);
        });
        for clipped in &output.shapes {
            collect_row_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        output.drop_without_applying_deltas();
        frame
    }

    /// Just `draw_rows`' `ScrollArea` content size for a render at `width`
    /// x `height` — the numeric half of the harness above, for tests that
    /// only care whether scrolling was needed (issue #84), not the painted
    /// geometry.
    fn rows_content_size(snapshot: &Snapshot, width: f32, height: f32) -> egui::Vec2 {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            ..Default::default()
        };
        let mut content_size = egui::Vec2::ZERO;
        let output = ctx.run_ui(input, |ui| {
            content_size = draw_rows(ui, snapshot, &Settings::default(), &icons, &mut None);
        });
        output.drop_without_applying_deltas();
        content_size
    }

    #[test]
    fn no_scrolling_needed_when_the_default_window_fits_every_row() {
        let snapshot = rows_test_snapshot(DEFAULT_VISIBLE_ROWS);
        let content = rows_content_size(&snapshot, default_inner_width(), default_inner_height());
        assert!(
            content.x <= default_inner_width() + 0.01,
            "content {content:?} must not exceed the {}pt default width",
            default_inner_width()
        );
        assert!(
            content.y <= default_inner_height() + 0.01,
            "content {content:?} must not exceed the {}pt default height",
            default_inner_height()
        );
    }

    #[test]
    fn scrolling_is_needed_once_rows_exceed_the_viewport_height() {
        let snapshot = rows_test_snapshot(DEFAULT_VISIBLE_ROWS + 5);
        let viewport_height = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
        let content = rows_content_size(&snapshot, default_inner_width(), viewport_height);
        assert!(
            content.y > viewport_height,
            "content {content:?} must exceed the {viewport_height}pt viewport for a scrollbar to be needed"
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
        let frame = rows_painted_boxes(&snapshot, default_inner_width(), default_inner_height());
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

    #[test]
    fn each_rows_name_text_is_vertically_centered_in_its_row() {
        let snapshot = rows_test_snapshot(DEFAULT_VISIBLE_ROWS);
        let frame = rows_painted_boxes(&snapshot, default_inner_width(), default_inner_height());
        let rows = frame.row_rects();
        assert_eq!(rows.len(), DEFAULT_VISIBLE_ROWS);
        for (i, row_rect) in rows.iter().enumerate() {
            let name = format!("P{i}");
            let text_rect = frame.text_box(&name);
            let diff = (text_rect.center().y - row_rect.center().y).abs();
            assert!(
                diff < 0.5,
                "row {i}: name center.y {} vs row center.y {} (diff {diff})",
                text_rect.center().y,
                row_rect.center().y
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
        };

        let frame = rows_painted_boxes_with(
            &snapshot,
            &settings,
            default_inner_width(),
            default_inner_height(),
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

    // -- window position tracking (issue #27) -----------------------------

    /// Calls `track_window_position` with `outer_rect`/`minimized` exactly
    /// as `OverlayApp::update` passes them post issue #134 review (read
    /// once by the caller and shared with `track_window_size`, rather than
    /// each tracker re-reading `ctx.input` itself). Returns everything it
    /// sent on the settings-writer channel.
    fn track_one_frame(
        settings: &mut Settings,
        outer_rect: Option<egui::Rect>,
        minimized: Option<bool>,
    ) -> Vec<Settings> {
        let (tx, rx) = crossbeam_channel::unbounded();
        track_window_position(outer_rect, minimized, settings, &tx);
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

    // -- window size tracking (issue #134) ---------------------------------

    /// Calls `track_window_size` with `inner_rect`/`minimized` exactly as
    /// `OverlayApp::update` passes them post issue #134 review (read once
    /// by the caller and shared with `track_window_position`, rather than
    /// each tracker re-reading `ctx.input` itself). Returns everything it
    /// sent on the settings-writer channel.
    fn track_size_one_frame(
        settings: &mut Settings,
        inner_rect: Option<egui::Rect>,
        minimized: Option<bool>,
    ) -> Vec<Settings> {
        let (tx, rx) = crossbeam_channel::unbounded();
        track_window_size(inner_rect, minimized, settings, &tx);
        drop(tx);
        rx.try_iter().collect()
    }

    fn inner_rect_of(width: f32, height: f32) -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(width, height),
        ))
    }

    #[test]
    fn track_window_size_persists_a_resized_window() {
        let mut settings = Settings::default();

        let sent = track_size_one_frame(&mut settings, inner_rect_of(640.0, 480.0), Some(false));

        assert_eq!(settings.window_size, Some([640.0, 480.0]));
        assert_eq!(sent.len(), 1, "one resize, one send");
        assert_eq!(sent[0].window_size, Some([640.0, 480.0]));
    }

    #[test]
    fn track_window_size_stays_quiet_when_the_window_has_not_resized() {
        let mut settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        let sent = track_size_one_frame(&mut settings, inner_rect_of(640.0, 480.0), Some(false));

        assert!(sent.is_empty(), "an unresized window must not send");
        assert_eq!(settings.window_size, Some([640.0, 480.0]));
    }

    /// A minimized window may report a meaningless (e.g. zeroed) inner
    /// size, so nothing reported while minimized is persisted.
    #[test]
    fn track_window_size_ignores_a_minimized_window() {
        let mut settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        let sent = track_size_one_frame(&mut settings, inner_rect_of(0.0, 0.0), Some(true));

        assert!(sent.is_empty(), "a minimized window must not send");
        assert_eq!(settings.window_size, Some([640.0, 480.0]));
    }

    /// Same zeroed-size failure mode, but reported before the `minimized`
    /// flag catches up — the plausibility floor is what rejects it.
    #[test]
    fn track_window_size_ignores_an_absurd_zeroed_size() {
        let mut settings = Settings {
            window_size: Some([640.0, 480.0]),
            ..Settings::default()
        };

        let sent = track_size_one_frame(&mut settings, inner_rect_of(0.0, 0.0), None);

        assert!(sent.is_empty(), "a bogus size must not send");
        assert_eq!(settings.window_size, Some([640.0, 480.0]));
    }

    // -- viewport() restore + sanity clamp (issue #134) --------------------

    #[test]
    fn viewport_applies_a_restored_size_when_some() {
        let built = viewport(None, Some([640.0, 480.0]));

        assert_eq!(built.inner_size, Some(egui::vec2(640.0, 480.0)));
    }

    #[test]
    fn viewport_applies_the_default_size_when_none() {
        let built = viewport(None, None);

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(default_inner_width(), default_inner_height()))
        );
    }

    #[test]
    fn viewport_falls_back_to_default_size_for_a_too_small_persisted_value() {
        // Below `MIN_INNER_SIZE` on both axes; the restored size must still
        // be clamped up to the floor rather than opening an unusable sliver.
        let built = viewport(None, Some([1.0, 1.0]));

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(MIN_INNER_SIZE.x, MIN_INNER_SIZE.y))
        );
    }

    #[test]
    fn viewport_falls_back_to_default_size_for_a_non_finite_persisted_value() {
        let built = viewport(None, Some([f32::NAN, 480.0]));

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(default_inner_width(), default_inner_height())),
            "a non-finite persisted size must be rejected outright, not clamped"
        );
    }

    #[test]
    fn viewport_falls_back_to_default_size_for_an_absurdly_large_persisted_value() {
        let built = viewport(None, Some([1.0e9, 480.0]));

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(default_inner_width(), default_inner_height())),
            "a corrupted, absurdly large persisted size must be rejected outright"
        );
    }

    #[test]
    fn viewport_falls_back_to_default_size_for_an_oversize_area_within_per_axis_bounds() {
        // Each axis alone is under `MAX_INNER_SIZE_DIMENSION` (20,000), but
        // the product is ~400 million points — the total-area bound is what
        // must reject this, not the per-axis one.
        let built = viewport(None, Some([19_999.0, 19_999.0]));

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(default_inner_width(), default_inner_height())),
            "a per-axis-plausible but absurdly large-area persisted size must be rejected outright"
        );
    }

    #[test]
    fn viewport_reset_ignores_any_persisted_size() {
        // `main.rs`'s tray "Reset Window" action calls `viewport(None,
        // None)` regardless of what's persisted — this just pins that the
        // default-size call path is unaffected by a persisted size.
        let reset = viewport(None, None);
        let with_persisted = viewport(None, Some([999.0, 999.0]));

        assert_ne!(reset.inner_size, with_persisted.inner_size);
        assert_eq!(
            reset.inner_size,
            Some(egui::vec2(default_inner_width(), default_inner_height()))
        );
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
    /// Matches a node's `label()` (what `Button`/`Checkbox`/interact-only
    /// widgets like `menu_chevron` register via `WidgetInfo::labeled`) or,
    /// failing that, its `value()` (where a plain `ui.label(...)` — a
    /// `Role::Label` node — puts its text instead; see egui's
    /// `Response::fill_widget_info`, which calls `set_value` rather than
    /// `set_label` specifically for `Role::Label`).
    fn accessible_rect_for_label(update: &egui::accesskit::TreeUpdate, label: &str) -> egui::Rect {
        let bounds = update
            .nodes
            .iter()
            .find_map(|(_, node)| {
                // A direct `==` rather than `.as_deref() == Some(label)`:
                // accesskit's `label()`/`value()` return type differs by
                // target (owned `String` on some, borrowed `&str` on
                // others — `x86_64-pc-windows-gnu`, this crate's CI
                // target, is one where `.as_deref()` is already a no-op
                // and clippy's `needless_option_as_deref` rejects it), and
                // `String`/`&str` both compare directly against `label: &str`.
                (node.label().is_some_and(|s| s == label)
                    || node.value().is_some_and(|s| s == label))
                .then(|| node.bounds())
                .flatten()
            })
            .unwrap_or_else(|| panic!("no accessible node labeled {label:?} painted"));
        egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        )
    }

    /// Same idea as `accessible_rect_for_label`, but for a widget like the
    /// opacity `Slider` that carries no label of its own
    /// (`.show_value(false)` and no `.text(...)` call, so
    /// `WidgetInfo::slider` leaves `label` empty) — it can only be found by
    /// its AccessKit `role`, which egui sets from `WidgetType::Slider` for
    /// every `Slider` regardless of whether it has a label
    /// (`Response::fill_accesskit_node_from_widget_info`).
    fn accessible_rect_for_role(
        update: &egui::accesskit::TreeUpdate,
        role: egui::accesskit::Role,
    ) -> egui::Rect {
        let bounds = update
            .nodes
            .iter()
            .find_map(|(_, node)| (node.role() == role).then(|| node.bounds()).flatten())
            .unwrap_or_else(|| panic!("no accessible node with role {role:?} painted"));
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

    /// A press-and-hold at `pos` (move, press — no release) for a single
    /// frame. Unlike `click_at`, this deliberately does *not* also release
    /// in the same frame: egui's drag bookkeeping
    /// (`crate::interaction::update_interactions`) sets a widget's
    /// `potential_drag_id` on `Pressed` and then clears it right back to
    /// `None` on a same-frame `Released` — a widget sensitive only to
    /// `Sense::drag()` (like `Slider`, which has no `Sense::click()`) never
    /// actually registers as dragged if `click_at`'s press-then-release
    /// both land in one `RawInput`, so its value never updates. A real drag
    /// presses on one frame and releases several frames later; this
    /// reproduces the press half of that so the drag is live for the
    /// `run_ui` call it's passed to.
    fn press_at(pos: egui::Pos2) -> egui::RawInput {
        egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        }
    }

    /// The release half of the gesture `press_at` starts — a separate frame,
    /// same reasoning as `press_at`'s doc comment.
    fn release_at(pos: egui::Pos2) -> egui::RawInput {
        egui::RawInput {
            events: vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        }
    }

    // -- issue #171: "Check for updates" --------------------------------

    /// `start_update_check` is what the header dropdown's click handler
    /// calls; the thread it spawns talks to the network (or, off-Windows,
    /// immediately reports `platform::http_get`'s stub error) — see
    /// `update_check`'s own tests for everything decidable without one.
    /// All this checks is the state transition the click handler relies
    /// on: a fresh request always starts out `Checking`, never landing in
    /// `Idle` or `Done` before its thread has even run.
    #[test]
    fn start_update_check_begins_in_the_checking_state() {
        assert!(matches!(
            start_update_check(),
            UpdateCheckState::Checking { .. }
        ));
    }

    /// `OverlayApp::poll_update_check` is `drain_snapshots`'s counterpart
    /// for the update-check channel (see `UpdateCheckState`'s doc comment
    /// for why it has to be a once-per-frame poll rather than something
    /// read only while the dropdown happens to be open). This drives it
    /// directly, standing in for the update-check thread with a plain
    /// `send` on the same channel `start_update_check` would have handed
    /// out.
    #[test]
    fn poll_update_check_picks_up_a_landed_result() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        app.update_check = UpdateCheckState::Checking { rx };
        tx.send(Ok(CheckOutcome::UpToDate)).unwrap();

        app.poll_update_check();

        assert!(matches!(
            app.update_check,
            UpdateCheckState::Done(Ok(CheckOutcome::UpToDate))
        ));
    }

    /// Counterpart to the test above: with nothing sent on the channel yet,
    /// a poll must leave the state exactly as `Checking` — not, say, some
    /// default/empty `Done` — so a still-in-flight request keeps rendering
    /// "Checking…" instead of silently resolving to nothing.
    #[test]
    fn poll_update_check_leaves_an_in_flight_request_checking() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let (_tx, rx) = crossbeam_channel::unbounded();
        app.update_check = UpdateCheckState::Checking { rx };

        app.poll_update_check();

        assert!(matches!(
            app.update_check,
            UpdateCheckState::Checking { .. }
        ));
    }

    /// The failure mode the two tests above can't reach: the update-check
    /// thread dies without ever sending (a panic inside the unsafe WinHTTP
    /// FFI, say), so its sender drops and the channel disconnects. That has
    /// to resolve to a `Done(Err(..))` rather than read as "still empty" —
    /// otherwise the state stays `Checking` forever and, because
    /// `draw_header_menu` disables the button while checking (see
    /// `draw_header_menu_disables_check_for_updates_while_one_is_in_flight`),
    /// the user can never retry without restarting the app.
    #[test]
    fn poll_update_check_resolves_a_disconnected_channel_to_an_error() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let (tx, rx) = crossbeam_channel::unbounded::<Result<CheckOutcome, String>>();
        app.update_check = UpdateCheckState::Checking { rx };
        drop(tx);

        app.poll_update_check();

        assert!(
            matches!(app.update_check, UpdateCheckState::Done(Err(_))),
            "a dropped sender must resolve out of Checking, got {:?}",
            app.update_check
        );
    }

    /// Two checks at once would race two threads to the same channel, so
    /// the button is disabled while one is in flight — and, since that's
    /// the only thing standing between the user and a pile-up, it's worth
    /// asserting rather than assuming. Read back off AccessKit (which is
    /// where `Response::fill_accesskit_node_common` records `!enabled` as
    /// `set_disabled`) rather than off the painted shapes, since a disabled
    /// button paints the same string a live one does.
    #[test]
    fn draw_header_menu_disables_check_for_updates_while_one_is_in_flight() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let (_tx, rx) = crossbeam_channel::unbounded();

        let mut disabled_while = |update_check: &mut UpdateCheckState| {
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
                    update_check,
                    &unused_log_export_sender(),
                );
            });
            let update = output
                .platform_output
                .accesskit_update
                .clone()
                .expect("accesskit was enabled for this frame");
            let disabled = update
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|s| s == "Check for updates"))
                .map(|(_, node)| node.is_disabled());
            output.drop_without_applying_deltas();
            disabled
        };

        assert_eq!(
            disabled_while(&mut UpdateCheckState::Checking { rx }),
            Some(true),
            "the button must be disabled while a check is in flight"
        );
        assert_eq!(
            disabled_while(&mut UpdateCheckState::Done(Err("boom".to_string()))),
            Some(false),
            "a resolved check must leave the button clickable again, so the user can retry"
        );
    }

    /// The error branch of the same render the two `Done(Ok(..))` tests
    /// below cover: whatever `check_for_update` (or `poll_update_check`'s
    /// disconnected-channel path) reports has to reach the dropdown as
    /// text, since it's the only signal the user gets that the check
    /// failed rather than silently did nothing.
    #[test]
    fn draw_header_menu_shows_the_reason_a_check_failed() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut update_check = UpdateCheckState::Done(Err("no network".to_string()));

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
                &mut update_check,
                &unused_log_export_sender(),
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();

        let expected = "Update check failed: no network".to_string();
        assert!(
            texts.contains(&expected),
            "expected {expected:?} among the painted text, got {texts:?}"
        );
    }

    /// Renders `draw_header_menu` directly (not through the popup — same
    /// reasoning as `draw_header_menu_dispatches_close_to_the_right_command`)
    /// with the state already `Done(Ok(UpToDate))`, and reads back every
    /// string it painted the same way `header_rendered_texts` does for
    /// `draw_header`, since none of this menu's items go through accesskit
    /// alone — `ui.label`/`ui.button` both paint a `Shape::Text` regardless.
    #[test]
    fn draw_header_menu_shows_up_to_date_with_the_running_version() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut update_check = UpdateCheckState::Done(Ok(CheckOutcome::UpToDate));

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
                &mut update_check,
                &unused_log_export_sender(),
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();

        let expected = format!("Up to date (v{})", env!("CARGO_PKG_VERSION"));
        assert!(
            texts.contains(&expected),
            "expected {expected:?} among the painted text, got {texts:?}"
        );
    }

    /// Same shape as the test above, but for the update-available branch:
    /// both the tag and the "Download" hyperlink's own label must be
    /// painted — the actual `href` isn't a painted string at all (it's a
    /// `ViewportCommand::OpenUrl` queued on click, not text), so this only
    /// covers what a render test can see.
    #[test]
    fn draw_header_menu_shows_update_available_with_the_tag_and_a_download_link() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut update_check = UpdateCheckState::Done(Ok(CheckOutcome::UpdateAvailable {
            tag: "v0.3.0".to_string(),
            url: "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0".to_string(),
        }));

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
                &mut update_check,
                &unused_log_export_sender(),
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();

        assert!(
            texts.contains(&"Update available: v0.3.0".to_string()),
            "expected the update-available line among the painted text, got {texts:?}"
        );
        assert!(
            texts.contains(&"Download".to_string()),
            "expected the Download hyperlink's label among the painted text, got {texts:?}"
        );
    }

    /// Reset moved out of this menu into the toggle cluster (issue #82;
    /// see `clicking_reset_sends_the_reset_command`), leaving Close as the
    /// only command this menu itself still dispatches. This drives a real
    /// click through `Response::clicked()`, the same path `draw_header_menu`
    /// itself checks, rather than calling `ctx.send_viewport_cmd` directly.
    #[test]
    fn draw_header_menu_dispatches_close_to_the_right_command() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        // Frame 1: lay the menu out with no input, and read back where
        // AccessKit says "Close" actually painted — its rect isn't knowable
        // ahead of a real `draw_header_menu` run.
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
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let close_pos = accessible_rect_for_label(&update, "Close").center();
        layout.drop_without_applying_deltas();

        // Frame 2: click Close.
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
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
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

    /// Drives a real drag on the opacity slider (issue #166) through
    /// `Response::changed()` the same way `draw_header_menu_dispatches_
    /// close_to_the_right_command` drives a real click on Close — nothing
    /// before this test exercised `opacity_response.changed()` itself
    /// (~line 2602), only the pure color math it feeds into.
    #[test]
    fn draw_header_menu_slider_drag_updates_settings_and_sends_on_tx_settings() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        assert_eq!(
            settings.opacity,
            Settings::OPACITY_MAX,
            "the default must start at full opacity for this test to prove a drag actually moved it"
        );

        // Frame 1: lay the menu out with no input, and read back where
        // AccessKit says the opacity slider actually painted — its rect
        // isn't knowable ahead of a real `draw_header_menu` run.
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
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let slider_rect = accessible_rect_for_role(&update, egui::accesskit::Role::Slider);
        layout.drop_without_applying_deltas();

        // Frame 2: press (not click — see `press_at`'s doc comment) the far
        // left edge of the slider's rail — egui's `Slider::slider_ui`
        // clamps a position outside the rail to its nearest end
        // (`remap_clamp`), so this reliably lands on `Settings::OPACITY_MIN`
        // regardless of the handle's start position, the same way the
        // pre-existing `panic!`-on-miss `click_at` calls elsewhere in this
        // file target a known, stable point rather than a computed one.
        let drag_pos = egui::pos2(slider_rect.left(), slider_rect.center().y);
        let output = ctx.run_ui(press_at(drag_pos), |ui| {
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
            );
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            settings.opacity,
            Settings::OPACITY_MIN,
            "dragging the slider to its left edge must lower settings.opacity to the floor"
        );
        let sent = rx_settings
            .try_recv()
            .expect("a changed slider must send the new settings on tx_settings");
        assert_eq!(sent.opacity, Settings::OPACITY_MIN);
        assert!(
            rx_settings.try_recv().is_err(),
            "one slider drag must not send more than once"
        );

        // Frame 3: release, finishing the gesture `press_at` started. The
        // pointer hasn't moved since frame 2, so the value doesn't change
        // again here — this only proves letting go of the slider doesn't
        // send a spurious second `tx_settings` update.
        let output = ctx.run_ui(release_at(drag_pos), |ui| {
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
            );
        });
        output.drop_without_applying_deltas();
        assert_eq!(settings.opacity, Settings::OPACITY_MIN);
        assert!(
            rx_settings.try_recv().is_err(),
            "releasing the slider without moving it must not send again"
        );
    }

    /// Issue #203: the header dropdown's "Reset to defaults" item must
    /// resize the window to fit `RESET_TO_DEFAULTS_VISIBLE_ROWS` rows (not
    /// the tray's own 20-row `TrayCommand::ResetWindow`) and reset opacity
    /// to `Settings::default_opacity()`, sending the updated settings on
    /// `tx_settings` the same way the opacity slider above already does.
    #[test]
    fn draw_header_menu_reset_to_defaults_resizes_and_resets_opacity() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        // Start away from both defaults so the click can prove it actually
        // changed something rather than coincidentally matching already.
        settings.set_opacity(0.4);
        assert_ne!(settings.opacity, Settings::default_opacity());

        // Frame 1: lay the menu out with no input, and read back where
        // AccessKit says "Reset to defaults" actually painted — its rect
        // isn't knowable ahead of a real `draw_header_menu` run.
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
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let reset_pos = accessible_rect_for_label(&update, "Reset to defaults").center();
        layout.drop_without_applying_deltas();

        // Frame 2: click "Reset to defaults".
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
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
            );
        });
        let viewport_commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert_eq!(
            settings.opacity,
            Settings::default_opacity(),
            "Reset to defaults must restore full opacity"
        );
        let sent = rx_settings
            .try_recv()
            .expect("Reset to defaults must send the updated settings on tx_settings");
        assert_eq!(sent.opacity, Settings::default_opacity());
        assert!(
            rx_settings.try_recv().is_err(),
            "one click must not send more than once"
        );
        let expected_size = egui::vec2(default_inner_width(), reset_to_defaults_inner_height());
        assert!(
            viewport_commands.contains(&egui::ViewportCommand::InnerSize(expected_size)),
            "Reset to defaults must resize to fit {RESET_TO_DEFAULTS_VISIBLE_ROWS} rows: \
             {viewport_commands:?}"
        );
    }

    /// Regression coverage for issue #93's actual fix, which the test above
    /// cannot see: it calls `draw_header_menu` directly, never through the
    /// `egui::Popup::menu(&chevron_response).close_behavior(CloseOnClickOutside)`
    /// wiring `draw_header` builds around it (~lines 473-477), so the
    /// `ui.close()` calls on Minimize/Close are no-ops in that harness — a
    /// popup that was never opened has nothing to close.
    ///
    /// This drives the real thing through `draw_header` across several
    /// frames of the same `egui::Context` (memory persists across `run_ui`
    /// calls the way it would across real app frames): open the chevron
    /// menu with a genuine click, expand the Columns disclosure, click a
    /// column checkbox, and confirm the popup is still open afterward — the
    /// whole point of `CloseOnClickOutside` plus no `ui.close()` on that
    /// path. Then click Close and confirm the popup closes (on the frame
    /// after, since `Ui::close` only takes effect for the next frame's
    /// `is_open` check) and the right command still goes out, even with the
    /// popup now in the mix.
    #[test]
    fn header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        // The Columns disclosure section's own arrow (`show_toggle_button`)
        // reads `openness()` once before this test's click toggles it and
        // once more right after, both in the same frame — with the default
        // nonzero `animation_time`, the second read still animates from
        // the first read's now-stale timestamp with zero elapsed time, so
        // it stays at 0.0 and the checkboxes never render that frame. A
        // zero animation time makes `AnimationManager::animate_bool` snap
        // straight to the target instead (its `elapsed / animation_time`
        // divide-by-zero produces a non-finite value, which its own
        // fallback resolves to the target), matching how instantly a real
        // click should feel anyway.
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(0);
        let mut gesture = WindowGesture::default();
        let mut update_check = UpdateCheckState::default();

        // Runs one frame of the real `draw_header` (chevron, popup wiring,
        // and all) and hands back this frame's accessibility tree, the same
        // ground truth `draw_header_menu_dispatches_close_to_the_right_command`
        // reads Close's position from.
        let mut frame = |mut input: egui::RawInput| -> egui::accesskit::TreeUpdate {
            // A fixed, bounded screen every frame — the same reasoning as
            // `header_painted_boxes`'s doc comment: without one, the
            // popup's own best-alignment logic (there is nothing to align
            // *inside*) has no stable anchor to measure against, and the
            // chevron/menu paint at a different, arbitrary offset each
            // frame, silently invalidating a position captured on an
            // earlier frame.
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(default_inner_width(), default_inner_height()),
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
        let is_open = |update: &egui::accesskit::TreeUpdate| {
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Close"))
        };

        // Frame 1: closed header, find the chevron.
        let update = frame(egui::RawInput::default());
        assert!(!is_open(&update), "the menu must start closed");
        let chevron_pos = accessible_rect_for_label(&update, "Menu").center();

        // Frame 2: click the chevron. `Popup::menu`'s `open_memory` toggles
        // and the popup paints in the very same frame (egui's `Popup::show`
        // opens before deciding whether to render), so Columns is already
        // visible here — collapsed, per its own `default_open(false)`. Its
        // *position* is not trustworthy yet though: `Popup::show` runs the
        // just-opened Area through a `sizing_pass` (no prior measured size
        // to align against), which lays the content out differently from
        // every later frame — so this frame only proves the menu opened.
        let update = frame(click_at(chevron_pos));
        assert!(is_open(&update), "clicking the chevron must open the menu");

        // Frame 3: no new input, just letting the popup settle into its
        // real, stable position now that a prior frame's size is on record.
        let update = frame(egui::RawInput::default());
        assert!(is_open(&update), "the menu must still be open once settled");
        let columns_pos = accessible_rect_for_label(&update, "Columns").center();

        // Frame 4: click Columns to expand its disclosure section.
        let update = frame(click_at(columns_pos));
        assert!(
            is_open(&update),
            "expanding Columns must not close the menu"
        );
        let first_column_label = ColumnKind::ALL[0].label();
        let checkbox_pos = accessible_rect_for_label(&update, first_column_label).center();

        // Frame 5: click a column checkbox. Issue #93's fix — no `ui.close()`
        // on this path, plus the root popup's `CloseOnClickOutside` — means
        // this must NOT dismiss the popup, unlike the old submenu flyout.
        let update = frame(click_at(checkbox_pos));
        assert!(
            is_open(&update),
            "a Columns checkbox click must leave the popup open"
        );
        assert!(
            rx_command.try_recv().is_err(),
            "a checkbox click must not dispatch a command"
        );

        // Frame 6: confirm it is still open on the frame after too — not
        // just within the click frame itself.
        let update = frame(egui::RawInput::default());
        assert!(
            is_open(&update),
            "the popup must stay open on the frame after the checkbox click"
        );
        let close_pos = accessible_rect_for_label(&update, "Close").center();

        // Frame 7: click Close. It calls `ui.close()` itself, so — unlike
        // the checkbox — this must close the popup, and still dispatch the
        // Quit command even though the click went through the real popup
        // wiring this time, not a direct `draw_header_menu` call.
        let _ = frame(click_at(close_pos));
        assert_eq!(
            rx_command.try_recv().expect("Close must send a command"),
            UiCommand::Quit
        );

        // Frame 8: `Ui::close` only closes for the *next* frame's `is_open`
        // check (the frame it's called on already painted before the close
        // decision runs) — so the popup must be gone by now.
        let update = frame(egui::RawInput::default());
        assert!(
            !is_open(&update),
            "Close must actually dismiss the popup by the following frame"
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
        assert!(gesture_end_needs_frame_recompute(
            resize,
            egui::ViewportId::ROOT
        ));
    }

    #[test]
    fn a_finished_move_does_not_need_a_frame_recompute() {
        // A pure move never changes the window's size, so there is nothing
        // for a DWM frame recompute to fix.
        assert!(!gesture_end_needs_frame_recompute(
            GestureKind::Move,
            egui::ViewportId::ROOT
        ));
    }

    #[test]
    fn a_resize_finished_in_a_child_viewport_needs_no_frame_recompute() {
        // Issue #218: the breakdown windows share this driver, but
        // `platform::force_frame_recompute` can only reach the root `HWND`
        // it cached at startup. A child's resize must not fire a
        // `SetWindowPos` at the root window, which did not resize.
        let resize = GestureKind::Resize(egui::ResizeDirection::West);
        let child = egui::ViewportId::from_hash_of("skill-1550");
        assert!(!gesture_end_needs_frame_recompute(resize, child));
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

    // -- click-through hit box (issue #167 rehash) --------------------------

    /// The cluster rect the header allocates for `toggle_cluster`, at a
    /// round origin so the expected bounds below are readable.
    fn toggle_cluster_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 10.0), egui::vec2(80.0, 20.0))
    }

    #[test]
    fn the_click_through_slot_sits_after_share_and_reset() {
        let cluster = toggle_cluster_rect();
        let slot = click_through_button_slot(cluster);
        // Same running cursor `toggle_cluster` walks: pad, Share, gap,
        // Reset, gap — then the glyph box itself.
        assert_eq!(slot.left(), cluster.left() + CLICK_THROUGH_SLOT_OFFSET_X);
        assert_eq!(slot.width(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(slot.height(), TOGGLE_CLICK_THROUGH_SIDE);
        assert_eq!(slot.center().y, cluster.center().y);
    }

    #[test]
    fn the_hit_box_pads_the_glyph_at_scale_one() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        assert_eq!(
            click_through_hit_box_px(button, 1.0),
            (
                (20.0 - CLICK_THROUGH_HIT_PAD) as i32,
                (30.0 - CLICK_THROUGH_HIT_PAD) as i32,
                (34.0 + CLICK_THROUGH_HIT_PAD) as i32,
                (44.0 + CLICK_THROUGH_HIT_PAD) as i32,
            )
        );
    }

    #[test]
    fn the_hit_box_scales_to_physical_pixels() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        // 2x DPI: every padded bound doubles.
        assert_eq!(click_through_hit_box_px(button, 2.0), (36, 56, 72, 92));
        // A fractional scale rounds rather than truncating.
        assert_eq!(click_through_hit_box_px(button, 1.5), (27, 42, 54, 69));
    }

    /// A bad `pixels_per_point` must never collapse the hit box: every bound
    /// would round to `0`, and `platform::Rect::contains` can't be true for a
    /// zero-area rect, so `WM_NCHITTEST` would answer `Transparent` for every
    /// point — including the button that turns click-through back off.
    #[test]
    fn a_degenerate_scale_falls_back_to_identity() {
        let button = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(34.0, 44.0));
        let identity = click_through_hit_box_px(button, 1.0);
        for bad in [0.0, -1.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let (left, top, right, bottom) = click_through_hit_box_px(button, bad);
            assert_eq!(
                (left, top, right, bottom),
                identity,
                "pixels_per_point {bad} should fall back to 1.0"
            );
            assert!(
                right > left,
                "hit box must have area (pixels_per_point {bad})"
            );
            assert!(
                bottom > top,
                "hit box must have area (pixels_per_point {bad})"
            );
        }
    }

    // -- per-player skill breakdown window (issue #16) --------------------

    /// A single click (move, press, release, all in one frame) at `pos`
    /// with `button` — `click_at`'s shape, generalized to any button, so a
    /// right-click gesture can be synthesized the same way.
    fn click_at_with_button(pos: egui::Pos2, button: egui::PointerButton) -> egui::RawInput {
        let modifiers = egui::Modifiers::NONE;
        egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    modifiers,
                },
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: false,
                    modifiers,
                },
            ],
            ..Default::default()
        }
    }

    /// Two-frame click-on-a-row harness: frame 1 lays `draw_rows` out with
    /// no input (egui's own click hit-testing is one-frame-lagged — a
    /// widget must already have existed, from a *prior* frame, for a click
    /// on it to register on this one — the same reason `draw_header_menu_
    /// dispatches_close_to_the_right_command` runs a layout frame before
    /// its click frame), and reads back where row 0's name actually
    /// painted; frame 2 (the same `Context`, so the widget IDs line up)
    /// sends a synthesized click there with `button` and returns whatever
    /// `draw_rows` reported opened this time — the seam `draw_row`'s
    /// widened `Sense::click()` plus its `secondary_clicked()` gate feed
    /// (issue #16, D1).
    fn opened_uid_after_click(snapshot: &Snapshot, button: egui::PointerButton) -> Option<i64> {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0));

        let mut discarded = None;
        let layout = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_rows(ui, snapshot, &Settings::default(), &icons, &mut discarded);
            },
        );
        let mut frame = RowFrame {
            texts: Vec::new(),
            meshes: Vec::new(),
        };
        for clipped in &layout.shapes {
            collect_row_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        let pos = frame.text_box(&snapshot.rows[0].name).center();
        layout.drop_without_applying_deltas();

        let mut opened: Option<i64> = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..click_at_with_button(pos, button)
            },
            |ui| {
                draw_rows(ui, snapshot, &Settings::default(), &icons, &mut opened);
            },
        );
        output.drop_without_applying_deltas();
        opened
    }

    #[test]
    fn a_right_click_on_a_row_opens_that_players_breakdown() {
        let snapshot = rows_test_snapshot(1);
        let uid = snapshot.rows[0].uid;
        let opened = opened_uid_after_click(&snapshot, egui::PointerButton::Secondary);
        assert_eq!(opened, Some(uid));
    }

    #[test]
    fn a_left_click_on_a_row_opens_nothing() {
        let snapshot = rows_test_snapshot(1);
        let opened = opened_uid_after_click(&snapshot, egui::PointerButton::Primary);
        assert_eq!(opened, None);
    }

    /// Same two-frame click harness as `opened_uid_after_click`, but through
    /// `draw_history`'s open-encounter branch (issue #216) — the seam that
    /// used to swallow every right-click behind a hardcoded `&mut None`.
    fn opened_uid_after_history_click(
        open: OpenEncounter,
        button: egui::PointerButton,
    ) -> Option<i64> {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let settings = Settings::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0));
        let name = open.snapshot.rows[0].name.clone();

        let mut state = HistoryUi {
            open: Some(open),
            ..HistoryUi::default()
        };
        let mut back_to_live = false;
        let mut discarded = None;
        let layout = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_history(
                    ui,
                    &mut state,
                    &settings,
                    &icons,
                    None,
                    &tx,
                    &mut back_to_live,
                    &mut discarded,
                );
            },
        );
        let mut frame = RowFrame {
            texts: Vec::new(),
            meshes: Vec::new(),
        };
        for clipped in &layout.shapes {
            collect_row_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        let pos = frame.text_box(&name).center();
        layout.drop_without_applying_deltas();

        let mut opened: Option<i64> = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..click_at_with_button(pos, button)
            },
            |ui| {
                draw_history(
                    ui,
                    &mut state,
                    &settings,
                    &icons,
                    None,
                    &tx,
                    &mut back_to_live,
                    &mut opened,
                );
            },
        );
        output.drop_without_applying_deltas();
        opened
    }

    #[test]
    fn a_right_click_on_a_historical_row_opens_that_players_breakdown() {
        let snapshot = rows_test_snapshot(1);
        let uid = snapshot.rows[0].uid;
        let open = OpenEncounter {
            id: 1,
            title: "Historical Fight".to_string(),
            subtitle: None,
            ended_at_ms: 0,
            snapshot,
        };

        let opened = opened_uid_after_history_click(open, egui::PointerButton::Secondary);

        assert_eq!(opened, Some(uid));
    }

    #[test]
    fn a_left_click_on_a_historical_row_opens_nothing() {
        let snapshot = rows_test_snapshot(1);
        let open = OpenEncounter {
            id: 1,
            title: "Historical Fight".to_string(),
            subtitle: None,
            ended_at_ms: 0,
            snapshot,
        };

        let opened = opened_uid_after_history_click(open, egui::PointerButton::Primary);

        assert_eq!(opened, None);
    }

    fn skill_window_state(pos: egui::Pos2) -> SkillWindowState {
        SkillWindowState {
            sort: skills::SkillSort::default(),
            pos,
            size: SKILL_WINDOW_SIZE,
            source: SkillWindowSource::Live,
            gesture: WindowGesture::default(),
        }
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

    fn skill_inner_rect_of(size: egui::Vec2) -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(egui::pos2(1.0, 1.0), size))
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

    /// One row per fight for the same uid, told apart by `damage` — the
    /// whole point of issue #216's per-window source is that a uid present
    /// in both fights resolves to the right one.
    fn skill_source_row(uid: i64, damage: i64) -> PlayerRow {
        PlayerRow {
            uid,
            damage,
            ..sample_row(None)
        }
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
    /// historical one is the history schema never storing per-skill totals.
    #[test]
    fn skill_window_empty_message_is_worded_for_the_window_s_source() {
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::History(7), 0),
            Some("No per-skill data recorded for this fight")
        );
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::Live, 0),
            Some("No damage recorded yet"),
            "an ongoing fight's rows are still coming — claiming nothing was \
             recorded for it would be wrong"
        );
        assert_eq!(skill_window_empty_message(SkillWindowSource::Live, 3), None);
        assert_eq!(
            skill_window_empty_message(SkillWindowSource::History(7), 3),
            None
        );
    }

    fn sample_skill_row(skill_id: i32) -> SkillRow {
        SkillRow {
            skill_id,
            damage: 1_000,
            share_pct: 50.0,
            crit_pct: 10.0,
            max_crit: 200,
            avg_crit: 150.0,
            avg_white: 90.0,
            avg: 100.0,
            hits: 10,
            crit_hits: 1,
            hits_per_min: 5.0,
        }
    }

    /// Two-frame click harness for `draw_skill_window`, the same shape as
    /// `opened_uid_after_click`: frame 1 lays the window out with no input
    /// and reads back where `value`'s text actually painted (not knowable
    /// ahead of a real run); frame 2 (the same `Context`, so the interact
    /// ids line up) sends a synthesized left click there and returns
    /// whatever this run reports the `X` glyph did, leaving `sort` mutated
    /// in place for the caller to inspect.
    fn click_skill_window_at(row: &PlayerRow, sort: &mut skills::SkillSort, value: &str) -> bool {
        click_skill_window(row, sort, |frame| frame.text_box(value).center())
    }

    /// The same two-frame harness, aimed by an arbitrary `locate` instead of
    /// by a painted string — the close button paints no text at all since
    /// issue #218 turned its `\u{2715}` into two line segments.
    fn click_skill_window(
        row: &PlayerRow,
        sort: &mut skills::SkillSort,
        locate: impl FnOnce(&RowFrame) -> egui::Pos2,
    ) -> bool {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SKILL_WINDOW_SIZE);

        let layout = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                draw_skill_window(
                    ui,
                    row,
                    sort,
                    SkillWindowSource::Live,
                    &icons,
                    1.0,
                    &mut WindowGesture::default(),
                );
            },
        );
        let mut frame = RowFrame {
            texts: Vec::new(),
            meshes: Vec::new(),
        };
        for clipped in &layout.shapes {
            collect_row_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        let pos = locate(&frame);
        layout.drop_without_applying_deltas();

        let mut clicked = false;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..click_at_with_button(pos, egui::PointerButton::Primary)
            },
            |ui| {
                clicked = draw_skill_window(
                    ui,
                    row,
                    sort,
                    SkillWindowSource::Live,
                    &icons,
                    1.0,
                    &mut WindowGesture::default(),
                );
            },
        );
        output.drop_without_applying_deltas();
        clicked
    }

    #[test]
    fn clicking_a_column_header_toggles_its_sort() {
        let row = PlayerRow {
            skills: vec![sample_skill_row(1550), sample_skill_row(1551)],
            ..sample_row(None)
        };
        let mut sort = skills::SkillSort::default();
        assert_eq!(sort.column, skills::SkillColumn::Damage);

        // "Skill name" is the `Name` header's plain (unselected) label —
        // the default sort is `Damage`, so this is never the active-sort
        // text `header_label` would instead paint.
        click_skill_window_at(&row, &mut sort, "Skill name");

        assert_eq!(sort.column, skills::SkillColumn::Name);
        assert!(
            sort.descending,
            "a newly-clicked column always starts descending (D9)"
        );
    }

    // -- Encounter history view (issue #39) ---------------------------------

    /// Builds a throwaway `OverlayApp` for the history-view tests below —
    /// none of them exercise capture/settings/command plumbing, so every
    /// channel is a fresh, otherwise-unused pair.
    fn history_test_app() -> OverlayApp {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        )
    }

    #[test]
    fn a_fresh_overlay_starts_in_the_live_view() {
        let app = history_test_app();
        assert!(matches!(app.view, OverlayView::Live));
    }

    #[test]
    fn opening_history_switches_the_view() {
        let mut app = history_test_app();
        app.open_history();
        assert!(matches!(app.view, OverlayView::History(_)));
    }

    #[test]
    fn a_listed_reply_populates_the_encounter_list() {
        let mut app = history_test_app();
        app.open_history();

        let summary = history::EncounterSummary {
            id: 1,
            ended_at_ms: 1_000,
            duration_ms: 5_000,
            total_damage: 1_000,
            total_dps: 200.0,
            title: "Test Boss".to_string(),
            subtitle: None,
            player_count: 3,
        };
        app.tx_history
            .send(history::writer::HistoryEvent::Listed(vec![summary]))
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert_eq!(state.encounters.len(), 1);
    }

    fn history_test_record(title: &str) -> history::EncounterRecord {
        history::EncounterRecord {
            ended_at_ms: 2_000,
            duration_ms: 5_000,
            total_damage: 1_000,
            total_dps: 200.0,
            boss_monster_id: None,
            boss_name: None,
            is_boss: false,
            scene_id: None,
            scene_name: None,
            title: title.to_string(),
            subtitle: None,
            meter_version: "0.0.0".to_string(),
            players: Vec::new(),
        }
    }

    /// The list has to ask for everything retention keeps, or encounters
    /// past the limit are stranded on disk: visible to no one, deletable
    /// only by "Clear all".
    #[test]
    fn the_list_limit_follows_the_configured_retention_cap() {
        let mut app = history_test_app();

        // Below the ceiling: the configured value is returned as-is.
        app.settings.history_max_encounters = 50;
        assert_eq!(app.history_list_limit(), 50);

        // Above the ceiling: clamped down to the ceiling.
        app.settings.history_max_encounters = Settings::HISTORY_MAX_ENCOUNTERS_CAP + 1;
        assert_eq!(
            app.history_list_limit(),
            Settings::HISTORY_MAX_ENCOUNTERS_CAP
        );

        // `0` is "prune nothing by count", which no `LIMIT` expresses —
        // falls back to the ceiling.
        app.settings.history_max_encounters = 0;
        assert_eq!(
            app.history_list_limit(),
            Settings::HISTORY_MAX_ENCOUNTERS_CAP
        );
    }

    #[test]
    fn a_loaded_reply_opens_that_encounter() {
        let mut app = history_test_app();
        app.open_history();
        let OverlayView::History(state) = &mut app.view else {
            panic!("expected the History view");
        };
        // What a real `HistoryRowAction::Open(7)` click would have set.
        state.pending_load_id = Some(7);

        app.tx_history
            .send(history::writer::HistoryEvent::Loaded {
                id: 7,
                record: Box::new(history_test_record("Loaded Fight")),
            })
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert_eq!(state.open.as_ref().map(|open| open.id), Some(7));
    }

    /// Rows stay clickable while a load is in flight, so clicking row 1 and
    /// then row 2 leaves two `Load` requests outstanding: row 1's reply must
    /// not open its record under row 2's id, and must not consume the
    /// pending id row 2's own reply is still waiting on.
    #[test]
    fn a_stale_loaded_reply_is_dropped() {
        let mut app = history_test_app();
        app.open_history();
        let OverlayView::History(state) = &mut app.view else {
            panic!("expected the History view");
        };
        state.pending_load_id = Some(2);

        app.tx_history
            .send(history::writer::HistoryEvent::Loaded {
                id: 1,
                record: Box::new(history_test_record("First Click")),
            })
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert!(state.open.is_none());

        app.tx_history
            .send(history::writer::HistoryEvent::Loaded {
                id: 2,
                record: Box::new(history_test_record("Second Click")),
            })
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        let open = state.open.as_ref().expect("the newest load should open");
        assert_eq!((open.id, open.title.as_str()), (2, "Second Click"));
    }

    /// The same staleness rule as `a_stale_loaded_reply_is_dropped`, for the
    /// `Missing` reply: an earlier click's "it's gone" must not close the
    /// encounter a later click is about to open.
    #[test]
    fn a_stale_missing_reply_is_dropped() {
        let mut app = history_test_app();
        app.open_history();
        let OverlayView::History(state) = &mut app.view else {
            panic!("expected the History view");
        };
        state.pending_load_id = Some(2);

        app.tx_history
            .send(history::writer::HistoryEvent::Missing(1))
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert_eq!(state.pending_load_id, Some(2));
    }

    #[test]
    fn a_missing_reply_drops_back_to_the_list() {
        let mut app = history_test_app();
        app.open_history();
        let OverlayView::History(state) = &mut app.view else {
            panic!("expected the History view");
        };
        state.pending_load_id = Some(9);
        state.open = Some(OpenEncounter {
            id: 9,
            title: "Stale Fight".to_string(),
            subtitle: None,
            ended_at_ms: 0,
            snapshot: header_test_snapshot(1_000),
        });

        app.tx_history
            .send(history::writer::HistoryEvent::Missing(9))
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert!(state.open.is_none());
    }

    #[test]
    fn a_failed_reply_is_surfaced_as_an_error() {
        let mut app = history_test_app();
        app.open_history();

        app.tx_history
            .send(history::writer::HistoryEvent::Failed("boom".to_string()))
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        assert_eq!(state.error.as_deref(), Some("boom"));
    }

    /// `draw_history_bar`'s "← Live" button is what `OverlayApp::ui` reads
    /// (as `back_to_live`) to actually reset `self.view` — see that call
    /// site's own comment for why the reset itself can't run inside a unit
    /// test (it needs a live `eframe::Frame`, which nothing in this test
    /// module can construct). This is the mechanism that drives it.
    #[test]
    fn back_to_live_restores_the_live_view() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let state = HistoryUi::default();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_history_bar(ui, &state);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let live_pos = accessible_rect_for_label(&update, "← Live").center();
        layout.drop_without_applying_deltas();

        let mut action = HistoryBarAction::None;
        let output = ctx.run_ui(click_at(live_pos), |ui| {
            action = draw_history_bar(ui, &state);
        });
        output.drop_without_applying_deltas();

        assert!(matches!(action, HistoryBarAction::Live));
    }

    #[test]
    fn the_header_prefers_the_historical_title() {
        let snapshot = header_test_snapshot(1_000);
        let history = HistoryHeader {
            title: "Historical Fight",
            subtitle: Some("Historical Scene"),
        };

        let (title, subtitle) = header_text(&snapshot, Some(&history));

        assert_eq!(
            (title, subtitle),
            (
                "Historical Fight".to_string(),
                Some("Historical Scene".to_string())
            )
        );
    }

    #[test]
    fn clicking_the_close_glyph_closes_the_window() {
        let row = PlayerRow {
            skills: vec![sample_skill_row(1550)],
            ..sample_row(None)
        };
        let mut sort = skills::SkillSort::default();

        // The close button is aimed at geometrically: it paints two line
        // segments now, not a glyph (issue #218).
        let closed = click_skill_window(&row, &mut sort, |_| {
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

    #[test]
    fn clear_all_requires_a_second_click() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let settings = Settings::default();
        let mut state = HistoryUi::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut back_to_live = false;

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_history(
                ui,
                &mut state,
                &settings,
                &icons,
                None,
                &tx,
                &mut back_to_live,
                &mut None,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let first_pos = accessible_rect_for_label(&update, "Clear all").center();
        layout.drop_without_applying_deltas();

        ctx.run_ui(click_at(first_pos), |ui| {
            draw_history(
                ui,
                &mut state,
                &settings,
                &icons,
                None,
                &tx,
                &mut back_to_live,
                &mut None,
            );
        })
        .drop_without_applying_deltas();
        assert!(
            state.confirm_clear,
            "the first click must only arm the confirm state, not fire"
        );

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_history(
                ui,
                &mut state,
                &settings,
                &icons,
                None,
                &tx,
                &mut back_to_live,
                &mut None,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let second_pos = accessible_rect_for_label(&update, "Clear all — confirm?").center();
        layout.drop_without_applying_deltas();

        ctx.run_ui(click_at(second_pos), |ui| {
            draw_history(
                ui,
                &mut state,
                &settings,
                &icons,
                None,
                &tx,
                &mut back_to_live,
                &mut None,
            );
        })
        .drop_without_applying_deltas();
        assert!(
            !state.confirm_clear,
            "the second click must fire and reset the confirm state"
        );
    }

    /// Issue #219: `draw_history` draws its own nav bar and a second
    /// separator before an open encounter's rows — the crop bound's
    /// `rows_top` must be measured *after* that chrome, not wherever the
    /// outer panel's own separator (before `draw_history` even runs) left
    /// off, or the last row(s) get cropped out of the Share screenshot.
    /// Returning it from `draw_history` itself, rather than the caller
    /// re-deriving it, is what keeps the two from drifting apart.
    #[test]
    fn draw_history_reports_rows_top_after_its_own_bar_and_separator() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let settings = Settings::default();
        let mut state = HistoryUi {
            open: Some(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(3),
            }),
            ..HistoryUi::default()
        };
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut back_to_live = false;

        let mut pre_call_top = 0.0;
        let mut reported_rows_top = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            pre_call_top = ui.cursor().top();
            let (rows_top, _rows_area_height) = draw_history(
                ui,
                &mut state,
                &settings,
                &icons,
                None,
                &tx,
                &mut back_to_live,
                &mut None,
            );
            reported_rows_top = rows_top;
        })
        .drop_without_applying_deltas();

        assert!(
            reported_rows_top > pre_call_top,
            "rows_top must move past the history bar and separator \
             (pre-call top {pre_call_top}, reported {reported_rows_top})"
        );
    }
}
