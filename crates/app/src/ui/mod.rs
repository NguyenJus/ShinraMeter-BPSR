//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in
//! — with three deliberate exceptions, all header-menu items: issue #171's
//! "Check for updates" (`draw_header_menu`, `UpdateCheckState`), issue
//! #220's "Export logs" (`start_log_export`), and "Export session bundle"
//! (`start_bundle_export`, `crate::bundle`) — the whole-folder handover (log
//! files, the packet-inspection dump ring if it was on, `settings.json`, and
//! a `manifest.json`) so an agent can triage a session without the
//! maintainer's help. Each spawns its own one-shot `std::thread` and reports
//! back over a `crossbeam_channel`, the same way `settings::spawn_writer`
//! and `pipeline::spawn` do at the app layer, because the app layer has no
//! channel of its own suited to a single manual, UI-triggered request/reply.
//! "Export session bundle" deliberately reuses "Export logs"' own
//! `LogExportOutcome`/`tx_log_export`/`poll_log_export` machinery rather
//! than adding a parallel copy of it — both report "a destination path, or
//! that path plus why it failed", and threading a second reply channel
//! through `draw_header`/`draw_header_menu`'s already-deep call chain (and
//! every test that calls either) would cost far more than the shared type
//! is worth.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bpsr_meter::{
    Class, EncounterInfo, PlayerRow, Role, SkillRow, SkillStats, Snapshot, skill_row_from_stats,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use eframe::egui;

use crate::bundle;
use crate::custom_image::{CustomImages, ImageError, ImageSlot};
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

mod header;
mod history_view;
mod menu;
mod opacity;
mod repaint;
mod settings;
mod skill_window;
mod status;
mod table;

pub(crate) use header::*;
pub(crate) use history_view::*;
pub(crate) use menu::*;
pub(crate) use opacity::Opacity;
pub(crate) use repaint::{RepaintInputs, repaint_policy};
pub(crate) use settings::*;
pub(crate) use skill_window::*;
pub(crate) use status::*;
pub(crate) use table::*;

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

/// Deterministic 32-bit FNV-1a — routes a skill id to a
/// `SKILL_PLACEHOLDER_PALETTE` swatch (issue #275). Picked over
/// `std::hash::DefaultHasher`/`RandomState` specifically because those are
/// *not* guaranteed stable across Rust versions or process runs, which
/// would break "the same id looks the same every time" — the one property
/// this hash exists to provide. Not used for anything else; not a general
/// hashing utility.
fn fnv1a_u32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Placeholder disc swatches for issue #275: twelve vivid, mid-dark solid
/// hues, chosen for two things at once. First, distance from the row
/// chrome's near-black neutrals (`SKILL_CHROME_FILL` 0x11/0x11/0x17,
/// `SKILL_PANEL_FILL` 0x21/0x21/0x27, `SKILL_COLUMN_HEADER_FILL`
/// 0x2a/0x2a/0x30) so a placeholder never reads as "nothing painted" the
/// way the old `SKILL_ICON_EMPTY` (0x33/0x33/0x3B, barely lighter than
/// that chrome) could. Second, being flat and saturated rather than
/// gradients or texture, so a placeholder never reads as photographic
/// game-art either — a user glancing at the row list should be able to
/// tell which rows have real vendored icons and which don't.
const SKILL_PLACEHOLDER_PALETTE: [(u8, u8, u8); 12] = [
    (0xB0, 0x3A, 0x2E), // brick red
    (0xC5, 0x6A, 0x1E), // burnt orange
    (0xD8, 0xB0, 0x2A), // gold
    (0x52, 0x97, 0x2E), // moss green
    (0x1C, 0x80, 0x6C), // teal
    (0x1E, 0x6F, 0x9E), // steel blue
    (0x3A, 0x4E, 0xB0), // indigo
    (0x6A, 0x3A, 0xB0), // violet
    (0xA5, 0x2E, 0x8C), // magenta
    (0x8C, 0x4A, 0x2E), // umber
    (0x4A, 0x5A, 0x6A), // slate
    (0xC0, 0x4A, 0x6A), // rose
];

/// WCAG relative luminance of an sRGB triple (0-255 channels) — used only
/// to pick a legible glyph color over a placeholder swatch (issue #275).
fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    fn channel(c: u8) -> f32 {
        let c = f32::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Background and glyph color for `skill_id`'s placeholder icon (issue
/// #275), as `(background, glyph)`. The background is `skill_id` — not the
/// name — routed through `fnv1a_u32` into `SKILL_PLACEHOLDER_PALETTE`: id,
/// because two ids that resolve to the same `skills::skill_monogram` (e.g.
/// every Lucky Strike weapon variant, all "LS") still need their own disc
/// color to stay told apart in a row list, which a name-derived color
/// could not give them.
///
/// The glyph color is whichever of near-black (`#1A1A1A`, matched to this
/// window's dark theme rather than a pure `#000`) or white gives the
/// higher WCAG contrast ratio against that background. `0.2017` is not a
/// round-number guess: it is the relative luminance at which the two
/// ratios cross, solved from `(l + 0.05)^2 = 1.05 * (l_black + 0.05)` where
/// `l_black` is `#1A1A1A`'s own luminance. Every current palette entry
/// clears 4.5:1 (WCAG AA's minimum for *normal* text, stricter than the
/// 3:1 large-text floor this glyph's ~15pt bold weight would otherwise
/// only need to clear) against its chosen glyph color — issue #281's
/// live-window pass found the original teal (4.25:1 against white) and
/// moss green (4.23:1 against black) both fell short of 4.5 despite
/// clearing 3:1, so both were darkened/lightened respectively until they
/// cleared 4.5 with a small margin, without changing which glyph color
/// either one picks.
///
/// Lives here rather than in `skills.rs` (moved in the issue #281 review
/// pass): `skills.rs`'s module doc says this file owns skill-name/sort
/// view-model logic and that "`ui/skill_window.rs` (T4) owns painting this; it must not
/// be touched here" — a WCAG contrast decision producing `egui::Color32`
/// paint values is exactly the painting decision that line rules out, so
/// it belongs beside `paint_skill_icon_placeholder`, the only caller.
fn skill_placeholder_colors(skill_id: i32) -> (egui::Color32, egui::Color32) {
    let idx = (fnv1a_u32(&skill_id.to_le_bytes()) as usize) % SKILL_PLACEHOLDER_PALETTE.len();
    let bg = SKILL_PLACEHOLDER_PALETTE[idx];
    let fg = if relative_luminance(bg) > 0.2017 {
        (0x1A, 0x1A, 0x1A)
    } else {
        (0xFF, 0xFF, 0xFF)
    };
    (
        egui::Color32::from_rgb(bg.0, bg.1, bg.2),
        egui::Color32::from_rgb(fg.0, fg.1, fg.2),
    )
}

/// Paints the issue #275 monogram placeholder that fills the skill-icon
/// column for an id with no upstream art: a flat disc in
/// `skill_placeholder_colors(skill_id)`'s background, with the 1-2
/// character monogram `skills::skill_monogram` derives from `name` centered
/// on top in the matching foreground. `name` is the skill's already
/// resolved display name (`skills::skill_display_name`), passed in rather
/// than re-resolved here, since the caller's per-row column loop also needs
/// it for the Name column cell and resolving it twice would repeat the
/// table lookup and its allocation for no reason.
///
/// Falls back to the original flat `SKILL_ICON_EMPTY` disc for the one case
/// `skill_monogram` returns `None` — a name with no derivable glyph at all
/// (blank, or pure punctuation) — since #275 found every id actually
/// observed in capture resolves to a real name (down to the `Skill #<id>`
/// fallback), so `SKILL_ICON_EMPTY` still exists for a hypothetical
/// genuinely-nameless id rather than for the 65 ids this issue is about.
///
/// Both the placeholder disc and its glyph are `.gamma_multiply(opacity)`'d.
/// That is a deliberate divergence from the real-icon branch beside this one
/// (`CLASS_ICON_TINT`, painted at full alpha per issue #166's row-content
/// rule) and from the old `SKILL_ICON_EMPTY` fallback this replaces (also
/// never faded) — but this content is new and *generated*, not real
/// content or chrome, and #252/#253 established that every visible surface
/// should track the opacity slider. A solid, saturated placeholder disc
/// pinned at full alpha while the rest of a low-opacity window fades would
/// make it the loudest element in the row list, which is the opposite of
/// reading as a fallback for missing art.
fn paint_skill_icon_placeholder(
    painter: &egui::Painter,
    center: egui::Pos2,
    skill_id: i32,
    name: &str,
    opacity: Opacity,
) {
    let Some(glyph) = skills::skill_monogram(name) else {
        painter.circle_filled(center, SKILL_ICON_PLACEHOLDER_RADIUS, SKILL_ICON_EMPTY);
        return;
    };
    let (bg, fg) = skill_placeholder_colors(skill_id);
    painter.circle_filled(center, SKILL_ICON_PLACEHOLDER_RADIUS, opacity.apply(bg));
    paint_bold_text(
        painter,
        center,
        egui::Align2::CENTER_CENTER,
        &glyph,
        SKILL_ICON_MONOGRAM_FONT_SIZE,
        opacity.apply(fg),
    );
}

/// Commands the overlay emits for the app layer to consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Reset,
    Quit,
    /// Issue #214: tear down what capture is tracking and re-run server
    /// detection from scratch. Routed through this channel like every other
    /// command, because the thing that has to act on it — the
    /// `CaptureHandle` — cannot be reached from the frame thread: it owns a
    /// raw Windows `HANDLE` and is neither `Send` nor `Sync`, which is why
    /// `CaptureHandle::request_restart` shipped with no caller at all.
    /// `pipeline::run` holds the `Send`-able half (`CaptureRestart`) and
    /// makes the request on this command's behalf.
    RestartCapture,
    /// The uids of every player with an open skill-breakdown window, sent
    /// whenever `OverlayApp::skill_windows`' key set changes (PR #268
    /// review, finding 2). `pipeline::run` keeps the latest one and uses it
    /// to skip building the heals/dealt/received/casts breakdowns
    /// (`bpsr_meter::Meter::snapshot_focused`) for every other player on
    /// the live ~10Hz publish tick — a skill window is closed almost all
    /// the time, so that work is otherwise wasted on every tick.
    SkillFocus(Vec<i64>),
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

/// What the banner says when the pipeline thread has died (issue #214).
/// Names a recovery rather than a cause: from the UI's seat the cause is
/// unknowable (the thread is simply gone), and the log line
/// `raise_pipeline_dead_status` writes alongside it is where the detail
/// belongs.
const PIPELINE_DEAD_STATUS: &str = "Meter pipeline stopped — restart ShinraMeter-BPSR to resume";

/// What the banner says when the capture thread has died but the pipeline
/// thread itself is still alive and publishing snapshots (pipeline-
/// robustness audit, finding 1). Distinct from `PIPELINE_DEAD_STATUS`
/// because the failure is: the snapshot channel never disconnects in this
/// scenario, so `raise_pipeline_dead_status` never fires — the overlay
/// looks alive but is frozen, and this is the only signal that says so.
const CAPTURE_DEAD_STATUS: &str = "Packet capture stopped — restart the meter to resume";

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
    /// The `skill_windows` key set as of the last `UiCommand::SkillFocus`
    /// sent to the pipeline thread (PR #268 review, finding 2). Compared
    /// against `skill_windows.keys()` each frame so the command only goes
    /// out on an actual open/close, not on every frame a window happens to
    /// be open.
    last_sent_skill_focus: Vec<i64>,
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
    /// Issue #321: set the moment `UiCommand::Quit` is sent — the Close
    /// menu item (`draw_header_menu`) and the in-place-update relaunch
    /// (`finish_update_install_with`) are the two sites that send it.
    /// `drain_snapshots` reads this when `rx_snapshot` disconnects to tell
    /// an orderly shutdown (the pipeline thread exiting its `run` loop
    /// right after `Quit`, which drops `tx_snapshot`) apart from the
    /// pipeline thread actually dying mid-session (issue #214's real
    /// failure mode) — only the latter should log at ERROR and raise the
    /// permanent "frozen" banner.
    quit_requested: bool,
    /// Issue #340: the rect `draw_header` actually painted on the last
    /// frame, or `None` before the first one. Measured once per frame at
    /// the single `draw_header` call site in `ui` and read back through
    /// `measured_header_band_height`, so the sizing math follows the real
    /// header instead of a constant that has to be kept in step with it by
    /// hand.
    header_rect: Option<egui::Rect>,
}

/// All icon textures the overlay paints, bundled so `OverlayApp` has exactly
/// one lazily-loaded field for them instead of one per icon set (issue #41).
///
/// Every set here is `include_bytes!`-ed at compile time (issue #123), so
/// `Icons::load` has nothing to resolve or warn about — it just decodes and
/// uploads a texture for each set.
pub(crate) struct Icons {
    pub(crate) classes: ClassIcons,
    pub(crate) toolbar: ToolbarIcons,
    pub(crate) glyphs: GlyphIcons,
    // IMAGINE-TAKEDOWN: one of five sites — see
    // `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
    pub(crate) imagines: ImagineIcons,
    /// Per-skill row icons for the breakdown window (issue #192).
    pub(crate) skills: SkillIcons,
    /// Issues #121/#253: the one exception to this struct's doc comment —
    /// the user's own header background and row backdrop, read off disk at
    /// runtime rather than `include_bytes!`-ed, and therefore loaded lazily
    /// (and re-loaded when the path or the region size changes) instead of
    /// once in `Icons::load`. See `custom_image` for the cache's shape.
    ///
    /// Behind a `RefCell` because every painter in this module holds
    /// `&Icons`, not `&mut Icons`: `draw_header`, `draw_header_wash`,
    /// `draw_header_menu` and `draw_rows` between them have some two dozen
    /// call sites, and threading a fresh `&mut` parameter through all of
    /// them to reach one lazily-populated cache would be a far larger and
    /// more merge-hostile change than confining the mutation here. The
    /// overlay's UI is single-threaded (one `eframe` frame callback), and
    /// every borrow taken in this module is released before the next —
    /// `custom_image_texture` in particular drops its guard before it
    /// paints — so the runtime check never actually fires.
    pub(crate) custom: RefCell<CustomImages>,
}

impl Icons {
    /// Safe to call more than once per process (each call re-decodes and
    /// re-uploads every icon), but nothing does: this module's
    /// `get_or_insert_with` call site only ever calls this on `OverlayApp`'s
    /// first `ui()` frame.
    pub(crate) fn load(ctx: &egui::Context) -> Self {
        Self {
            classes: ClassIcons::load(ctx),
            toolbar: ToolbarIcons::load(ctx),
            glyphs: GlyphIcons::load(ctx),
            imagines: ImagineIcons::load(ctx),
            skills: SkillIcons::load(ctx),
            // Issues #121/#253: nothing to load here — the user's images
            // are resolved on the first frame that paints a region they
            // are configured for, at the size that region turns out to be.
            custom: RefCell::new(CustomImages::default()),
        }
    }
}

/// Alpha of the scrim painted over a user's background image, before rows
/// or header text go on top of it (issues #121, #253).
///
/// #253 asks for "a dimming scrim or minimum-contrast rule … the way
/// `SKILL_ROW_HOVER_FILL` already layers over chrome rather than content".
/// This is that rule, and it is the whole legibility story: a user can
/// point either region at an arbitrary photograph, which may be white,
/// high-contrast, or busy exactly where the DPS numbers land, and no choice
/// of text color survives all of those. Compositing the image ~55% of the
/// way back towards `PANEL_FILL` bounds how bright the brightest possible
/// backdrop can get, so the row text keeps its contrast against *any*
/// image while the artwork is still plainly visible.
///
/// Deliberately a scrim over the image rather than a cap on the image's own
/// alpha: fading the image toward the panel fill by lowering its alpha
/// would make it disappear entirely as the opacity slider comes down (the
/// panel fill is fading too), whereas a scrim painted at the *same*
/// multiplied opacity keeps the image/scrim ratio — and so the contrast
/// guarantee — constant at every slider position.
const BACKGROUND_IMAGE_SCRIM_ALPHA: u8 = 0x8C;

/// The scrim itself: `PANEL_FILL`'s color at `BACKGROUND_IMAGE_SCRIM_ALPHA`,
/// so a dimmed custom image tends towards the same near-black the default
/// panel already is rather than towards some second, unrelated tone.
/// Spelled out rather than derived from `PANEL_FILL` because `Color32`'s
/// channel accessors are not `const fn`; the
/// `background_image_scrim_matches_the_panel_fill_color` test is what keeps
/// the two from drifting apart.
const BACKGROUND_IMAGE_SCRIM: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(18, 18, 22, BACKGROUND_IMAGE_SCRIM_ALPHA);

/// The tint a user's background image is blitted with: white (i.e. the
/// image's own colors, untouched) scaled by the overlay's opacity setting.
///
/// This is the rule issue #253 makes a hard requirement — the same
/// `.gamma_multiply(settings.opacity)` `PANEL_FILL` and every other
/// background surface already uses — hoisted into one pure function so both
/// regions get it and neither can quietly stop applying it. `Color32`
/// stores channels premultiplied, so scaling white this way scales the
/// blit's alpha, which is exactly "fade the image with the slider".
fn background_image_tint(opacity: Opacity) -> egui::Color32 {
    opacity.apply(egui::Color32::WHITE)
}

/// Resolves the texture to paint for `slot` over `rect` — together with
/// the source rectangle it holds, which `custom_image::cover_uv` needs to
/// build the blit's UV — or `None` for "paint the default artwork": no
/// path configured, or the configured one did not load (issues #121,
/// #253).
///
/// The size handed to the cache is the region's *true* pixel size. The
/// bucketing that keeps a resize drag from re-decoding happens inside the
/// cache, on the key alone; letting it reach the crop geometry is what
/// stretched the image on both regions before this returned a second
/// value.
///
/// Also the cache's eviction point: a slot with no path clears its entry,
/// so an image the user just removed stops holding GPU memory immediately
/// rather than until the process exits.
///
/// The `RefCell` guard is dropped before this returns (a `TextureId` is
/// `Copy`), so no caller can still be holding it while painting — see
/// `Icons::custom` for why that matters.
fn custom_image_texture(
    ctx: &egui::Context,
    icons: &Icons,
    slot: ImageSlot,
    settings: &Settings,
    rect: egui::Rect,
) -> Option<(egui::TextureId, [u32; 2])> {
    let mut cache = icons.custom.borrow_mut();
    let Some(path) = settings.background_image(slot) else {
        cache.clear(slot);
        return None;
    };
    let region = crate::custom_image::region_pixels(rect.size(), ctx.pixels_per_point());
    cache.texture(ctx, slot, path, region)
}

/// Paints a user's background image over `rect` — the image itself at the
/// overlay's opacity, then the legibility scrim at the same opacity —
/// returning whether anything was painted. `false` means the caller should
/// fall back to its compiled-in artwork.
///
/// One function for both regions so the opacity rule and the scrim rule are
/// written once: #253 is explicit that the backdrop must not ship at a
/// fixed opacity, and #121's header image is held to the same rule.
fn paint_background_image(
    painter: &egui::Painter,
    icons: &Icons,
    slot: ImageSlot,
    settings: &Settings,
    rect: egui::Rect,
) -> bool {
    let Some((texture, content)) = custom_image_texture(painter.ctx(), icons, slot, settings, rect)
    else {
        return false;
    };
    // Not a full-UV blit: the texture's own size is rounded up to
    // `custom_image`'s cache bucket, so its aspect ratio is not this rect's
    // and painting all of it into `rect` would scale the two axes by
    // different factors — the stretch this parameter exists to undo (see
    // `custom_image::cover_uv`). Computed from the live rect every frame,
    // since the cached texture deliberately outlives small size changes.
    painter.image(
        texture,
        rect,
        crate::custom_image::cover_uv(content, rect.size()),
        background_image_tint(Opacity::new(settings.opacity)),
    );
    painter.rect_filled(
        rect,
        0.0,
        Opacity::new(settings.opacity).apply(BACKGROUND_IMAGE_SCRIM),
    );
    true
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

/// The healing a demo row's class puts out (issue #245), in the same
/// `DemoSkill` shape the damage fixtures use. Only the two healer classes
/// have any — a `Marksman` healing for a third of their damage would be a
/// prettier demo and a less honest one.
fn demo_heals(class: Class) -> &'static [DemoSkill] {
    match class {
        // "Life Bloom" / "Verdant Grace" — ids from `tables::skill_name`.
        Class::VerdantOracle => &[
            (2_003, 41_200_000, 210, 62, 16_800_000, 402_000),
            (2_001, 18_900_000, 96, 25, 6_200_000, 331_000),
        ],
        Class::BeatPerformer => &[(2_401, 27_400_000, 158, 44, 9_900_000, 288_000)],
        _ => &[],
    }
}

/// That fixture list's total, i.e. the Heal tab's `% Heal` denominator.
fn heal_total(class: Class) -> i64 {
    demo_heals(class)
        .iter()
        .map(|&(_, amount, ..)| amount)
        .sum()
}

/// The "Skill dealt" tab's demo fixture (issue #245, PR #268 review finding
/// 3): `skills` and `heals` merged by skill id via `SkillStats::merge`,
/// exactly like the real `dealt_rows` (`crates/meter/src/encounter.rs`)
/// merges `stats.skills` and `stats.heals` — not concatenated, which would
/// leave two rows standing for one skill id that happens to appear in both
/// fixture lists rather than summing them into one. `total` is the row's
/// combined damage+heal total, the same "% Dealt" denominator the real
/// `dealt_rows` passes through from `stats.total_damage + stats.total_heal`.
fn demo_dealt_rows(
    skills: &[DemoSkill],
    heals: &[DemoSkill],
    total: i64,
    duration_ms: u64,
) -> Vec<SkillRow> {
    let mut merged: std::collections::HashMap<i32, SkillStats> = std::collections::HashMap::new();
    for &(skill_id, damage, hits, crit_hits, crit_damage, max_crit) in
        skills.iter().chain(heals.iter())
    {
        let entry = SkillStats {
            total_damage: damage,
            hits,
            crit_hits,
            crit_damage,
            max_crit,
            ..Default::default()
        };
        merged.entry(skill_id).or_default().merge(&entry);
    }
    let mut rows: Vec<SkillRow> = merged
        .iter()
        .map(|(&skill_id, stats)| skill_row_from_stats(skill_id, stats, total, duration_ms))
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.damage));
    rows
}

/// What the demo's boss lands on each row (issue #245) — the Skill
/// received tab's fixture. Three monster abilities, one of them a
/// heavy-hitting crit, which is the shape a tank's received breakdown has.
const DEMO_RECEIVED: &[DemoSkill] = &[
    (1_120_101, 4_820_000, 34, 6, 1_640_000, 412_000),
    (1_120_104, 2_310_000, 12, 3, 980_000, 388_000),
    (1_120_107, 640_000, 4, 0, 0, 0),
];

/// `DEMO_RECEIVED`'s total, i.e. the Skill received tab's `% Amt`
/// denominator.
fn demo_received_total() -> i64 {
    DEMO_RECEIVED.iter().map(|&(_, amount, ..)| amount).sum()
}

/// The Skill casts tab's demo fixture (issue #245): one cast per three
/// recorded hits of each damaging skill, floored at one. A real cast count
/// is never derivable from a hit count — that is exactly why the tab needs
/// its own packet source — but the demo only has to look like a plausible
/// pull, and a ratio keeps the two columns telling a consistent story.
fn demo_cast_rows(skills: &[DemoSkill], duration_ms: u64) -> Vec<SkillRow> {
    let mut rows: Vec<SkillRow> = skills
        .iter()
        .map(|&(skill_id, _, hits, ..)| {
            let stats = SkillStats {
                hits: (hits / 3).max(1),
                ..Default::default()
            };
            skill_row_from_stats(skill_id, &stats, 0, duration_ms)
        })
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.hits));
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
                    // A plausible corpse run per death (issue #254): the
                    // demo snapshot exists to show the chrome, and a
                    // death-time pill reading 00:00 next to a nonzero
                    // Deaths count would show the wrong chrome.
                    dead_ms: Some(u64::from(deaths) * 12_000),
                    skills: demo_skill_rows(demo_skills, damage, duration_ms),
                    // Issue #245: the demo seed exercises the breakdown
                    // window's other tabs too, so `--demo` shows a
                    // populated tab strip rather than five empty ones.
                    // Healing is only seeded for the two healer classes,
                    // which is the shape a real pull has; "dealt" is
                    // damage plus that healing, exactly as the meter's
                    // `dealt_rows` merges them, and "received" is the
                    // boss's swings landing back on the row.
                    heals: demo_skill_rows(demo_heals(class), heal_total(class), duration_ms),
                    dealt: demo_dealt_rows(
                        demo_skills,
                        demo_heals(class),
                        damage + heal_total(class),
                        duration_ms,
                    ),
                    received: demo_skill_rows(DEMO_RECEIVED, demo_received_total(), duration_ms),
                    // A cast count per damaging skill, deliberately a
                    // little above its hit count — most BPSR skills land
                    // more than one hit per cast, so equal figures would
                    // be the odd-looking ones.
                    casts: demo_cast_rows(demo_skills, duration_ms),
                    // Issue #267: the Buff tab has no demo seed yet — left
                    // empty rather than fabricated, same as every other
                    // untracked-until-now tab was before its own issue
                    // seeded one.
                    buffs: Vec::new(),
                    // Issue #338: no demo seed for the absorbed/immune
                    // channels either — same rationale as `buffs` above.
                    absorbed_total: 0,
                    immune_total: 0,
                    shield: None,
                }
            },
        )
        .collect();
    Snapshot {
        duration_ms,
        total_damage: row_damage_sum,
        total_dps: row_damage_sum as f64 / (duration_ms as f64 / 1000.0),
        total_absorbed: 0,
        total_immune: 0,
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
        local_uid: None,
        capture_alive: true,
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
            total_absorbed: 0,
            total_immune: 0,
            rows: Vec::new(),
            encounter: EncounterInfo::default(),
            local_uid: None,
            capture_alive: true,
        }
    }
}

impl OverlayApp {
    /// The overlay's current window opacity.
    ///
    /// The `OverlayApp`-level accessor for turning `Settings::opacity`'s raw
    /// `f32` into an [`Opacity`]; most paint paths that already hold `self`
    /// take the typed value from here. Sites that only have `&Settings`
    /// (not `&OverlayApp`) call `Opacity::new(settings.opacity)` directly
    /// instead — this is not the sole call site of `Opacity::new`.
    pub(crate) fn opacity(&self) -> Opacity {
        Opacity::new(self.settings.opacity)
    }

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
            header_rect: None,
            update_check: UpdateCheckState::Idle,
            rx_log_export,
            tx_log_export,
            screenshot_capture_frames_waited: 0,
            demo_mode,
            startup_toggles_applied: false,
            skill_windows: std::collections::BTreeMap::new(),
            last_sent_skill_focus: Vec::new(),
            history,
            rx_history,
            tx_history,
            view: OverlayView::Live,
            quit_requested: false,
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
    ///
    /// Issue #349: the repaint decision no longer depends on this
    /// function's return value — the pipeline thread now wakes the overlay
    /// itself the moment it publishes a changed snapshot (see
    /// `pipeline::RepaintHandle`), so `ui()`'s own repaint gate only needs
    /// `rx_snapshot`'s current emptiness (`snapshot_activity` below), not
    /// whether this particular frame happened to drain one.
    fn drain_snapshots(&mut self) {
        if self.demo_mode {
            return;
        }
        loop {
            match self.rx_snapshot.try_recv() {
                Ok(snap) => {
                    // Pipeline-robustness audit, finding 1: a dead capture
                    // thread never disconnects this channel — the pipeline
                    // thread is still alive and still publishing — so the
                    // `Disconnected` arm below cannot see it. `capture_alive`
                    // is how the pipeline says so instead; see
                    // `raise_capture_dead_status`.
                    if !snap.capture_alive {
                        self.raise_capture_dead_status();
                    }
                    self.snapshot = snap;
                }
                // Nothing new this frame — the overwhelmingly common case.
                Err(TryRecvError::Empty) => break,
                // Issue #214: the pipeline thread is gone. `try_iter`, which
                // this replaced, collapsed this into `Empty`, so a panicked
                // pipeline looked exactly like a quiet one and the overlay
                // went on painting the last snapshot forever — a frozen
                // meter with no banner, no log line, and nothing the user
                // could do but guess. `main.rs` never joins the thread
                // either, so this dropped `Sender` is the only signal that
                // reaches the UI at all.
                //
                // Issue #321: `run`'s own `Quit`/disconnect break drops
                // `tx_snapshot` immediately, so an orderly shutdown hits
                // this exact same `Disconnected` arm — one more egui frame
                // painted before the window actually closes is enough to
                // see it. `quit_requested` (set at both `UiCommand::Quit`
                // send sites) is what tells that apart from #214's real
                // failure mode, so only the latter still logs at ERROR and
                // raises the permanent banner.
                Err(TryRecvError::Disconnected) => {
                    if self.quit_requested {
                        log::info!("quit requested; pipeline shut down");
                    } else {
                        self.raise_pipeline_dead_status();
                    }
                    break;
                }
            }
        }
    }

    /// Raises the *permanent* banner for a dead pipeline thread (#214).
    ///
    /// Permanent (no `status_expires_at`) because nothing in the process
    /// restarts that thread: unlike the Share clipboard blip, this is still
    /// true on every later frame. Logged and raised exactly once — the
    /// early return below sees the banner it just set — so a disconnect
    /// does not write a line per frame at ~10 Hz for the rest of the
    /// session.
    ///
    /// A *permanent* error banner that is already up wins. The capture-init
    /// failure `main.rs` seeds through `with_status` names an actual cause
    /// ("WinDivert is not installed"), which is strictly more useful than
    /// "the pipeline stopped" — and a pipeline whose capture never started
    /// will disconnect at shutdown like any other.
    ///
    /// A *transient* error banner (`status_expires_at.is_some()` — e.g. a
    /// failed Share/Export logs copy) does not win: it is scheduled to clear
    /// itself in `expire_transient_status`, which runs after
    /// `drain_snapshots` in `ui()`'s per-frame order, so without this check
    /// a dead pipeline discovered while that banner is showing would stay
    /// hidden behind it for up to `TRANSIENT_STATUS_LINGER` before the fatal
    /// banner finally appears. Overriding clears `status_expires_at` too, or
    /// the permanent banner would inherit the transient one's expiry and
    /// vanish on schedule.
    fn raise_pipeline_dead_status(&mut self) {
        if matches!(self.status, StatusLine::Error(_)) && self.status_expires_at.is_none() {
            return;
        }
        log::error!(
            "the pipeline thread is gone (its snapshot channel disconnected); the meter is frozen \
             for the rest of this session"
        );
        self.status = StatusLine::Error(PIPELINE_DEAD_STATUS.to_string());
        self.status_expires_at = None;
    }

    /// Raises the *permanent* banner for a dead capture thread (pipeline-
    /// robustness audit, finding 1) — the counterpart to
    /// `raise_pipeline_dead_status` for the failure that mechanism cannot
    /// see: the capture thread died, but the pipeline thread is still alive
    /// and still publishing snapshots on schedule, so the snapshot channel
    /// never disconnects. `drain_snapshots` calls this for every snapshot
    /// it drains once `Snapshot::capture_alive` goes `false`, which is
    /// permanently, so the same "logged/raised once" shape applies: the
    /// early return below sees the banner it just set.
    ///
    /// Same precedence rule as `raise_pipeline_dead_status`: an existing
    /// *permanent* banner wins (a named cause, or this same banner already
    /// up), a *transient* one does not.
    fn raise_capture_dead_status(&mut self) {
        if matches!(self.status, StatusLine::Error(_)) && self.status_expires_at.is_none() {
            return;
        }
        log::error!(
            "the capture thread is gone (its event channel disconnected); the pipeline is still \
             publishing snapshots but they will never change again for the rest of this session"
        );
        self.status = StatusLine::Error(CAPTURE_DEAD_STATUS.to_string());
        self.status_expires_at = None;
    }

    /// Issue #171: picks up the manual update-check thread's result, if one
    /// is in flight and has landed — the counterpart to `drain_snapshots`,
    /// called once per frame from `ui()` so a reply that arrives while the
    /// header dropdown happens to be closed is still there the moment it's
    /// reopened, rather than dropped or leaving the dropdown stuck showing
    /// "Checking…" forever.
    fn poll_update_check(&mut self, ctx: &egui::Context) {
        let landed = match &self.update_check {
            UpdateCheckState::Checking { rx } => match rx.try_recv() {
                Ok(outcome) => Some(LandedUpdate::Check(outcome)),
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
                Err(TryRecvError::Disconnected) => Some(LandedUpdate::Check(Err(
                    "the update-check thread stopped without reporting a result".to_string(),
                ))),
            },
            // Issue #250: the same drain, for the thread that downloads and
            // swaps in the new executable. Same `Disconnected` reasoning as
            // above — a thread that dies mid-download must resolve to a
            // visible failure, not leave the dropdown stuck on
            // "Downloading…" with no way back.
            UpdateCheckState::Installing { available, rx } => match rx.try_recv() {
                Ok(result) => Some(LandedUpdate::Install {
                    available: available.clone(),
                    result,
                }),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(LandedUpdate::Install {
                    available: available.clone(),
                    result: Err(
                        "the update-install thread stopped without reporting a result".to_string(),
                    ),
                }),
            },
            _ => None,
        };
        match landed {
            None => {}
            Some(LandedUpdate::Check(outcome)) => {
                self.update_check = UpdateCheckState::Done(outcome);
            }
            Some(LandedUpdate::Install { available, result }) => {
                self.update_check = self.finish_update_install(ctx, available, result);
            }
        }
    }

    /// Issue #250: what to do the frame an in-place update finishes.
    ///
    /// The download and the file swap already happened on the install
    /// thread (`update_check::install_update`); everything left is process
    /// lifecycle, which has to happen here because only the UI thread may
    /// send a viewport command. On success: start the new executable, tell
    /// the pipeline to stop, and ask the window to close — in that order,
    /// so a relaunch that fails leaves the current session running rather
    /// than closing the app over a build the user now has to start by hand.
    ///
    /// Returns the state to store rather than assigning it, so the borrow
    /// of `self.update_check` in `poll_update_check`'s match is already
    /// over by the time it is replaced.
    fn finish_update_install(
        &mut self,
        ctx: &egui::Context,
        available: CheckOutcome,
        result: Result<PathBuf, String>,
    ) -> UpdateCheckState {
        self.finish_update_install_with(ctx, available, result, update_check::relaunch)
    }

    /// The body of `finish_update_install`, with the actual relaunch call
    /// taken as a parameter instead of hard-coded to `update_check::relaunch`.
    /// `finish_update_install` is the only production caller and always
    /// passes `update_check::relaunch`, so behavior is unchanged; tests use
    /// this seam to drive the success branch without spawning a real
    /// process.
    fn finish_update_install_with(
        &mut self,
        ctx: &egui::Context,
        available: CheckOutcome,
        result: Result<PathBuf, String>,
        relaunch: impl FnOnce(&Path) -> Result<(), String>,
    ) -> UpdateCheckState {
        let installed = match result {
            Ok(installed) => installed,
            Err(err) => {
                return UpdateCheckState::InstallFailed {
                    available,
                    error: err,
                };
            }
        };
        if let Err(err) = relaunch(&installed) {
            // The swap succeeded, so the executable on disk *is* the new
            // build — only starting it failed. Say so explicitly: telling
            // the user the update failed would be wrong, and re-running the
            // download would be pointless work.
            log::error!("installed the update but couldn't relaunch: {err}");
            return UpdateCheckState::InstallFailed {
                available,
                error: format!(
                    "the update was installed but couldn't be started ({err}) — close the meter and open it again"
                ),
            };
        }
        log::info!(
            "installed an in-place update at {} and relaunched; closing this instance",
            installed.display()
        );
        let _ = self.tx_command.try_send(UiCommand::Quit);
        self.quit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        UpdateCheckState::Restarting
    }

    /// Issue #220 (PR #227 review): picks up whatever the "Export logs" or
    /// "Export session bundle" threads have finished — both report through
    /// the same `LogExportOutcome`/`rx_log_export` (see this module's own
    /// doc comment for why). Like `poll_update_check`, drained once per
    /// frame whether or not the dropdown is open — either item closes the
    /// dropdown on click, so by the time a multi-megabyte copy finishes
    /// there is no menu left to report through.
    ///
    /// A failure lands on the panel's existing transient error banner (the
    /// same one the Share clipboard failure uses) as well as the log: the
    /// log-only reporting this used to do told a user whose export silently
    /// produced nothing exactly nothing, in the one situation where they
    /// are already trying to hand over a log. A clean success is logged
    /// only — `StatusLine` has no non-error state to say it with, and the
    /// file/folder appearing where the user just chose to put it is its own
    /// feedback. A bundle that landed but couldn't copy every file
    /// (`bundle::export_bundle_to`'s missing list — a dump chunk the writer
    /// rotated away mid-export, say) does use the banner: it looks
    /// identical to a clean export on disk, so nothing else would tell the
    /// user their handover is short.
    fn poll_log_export(&mut self, now: Instant) {
        // Collected before the loop rather than iterated in place: the
        // failure arm below needs `&mut self`, which a live borrow of
        // `self.rx_log_export` would rule out.
        let landed: Vec<LogExportOutcome> = self.rx_log_export.try_iter().collect();
        for outcome in landed {
            match outcome {
                Ok((dest, missing)) if missing.is_empty() => {
                    log::info!("export finished: {}", dest.display())
                }
                // A bundle that came up short still landed, so this is not
                // an "Export failed" — but it must not pass silently
                // either: the whole point of a bundle is that whoever
                // receives it can tell what's in it.
                Ok((dest, missing)) => {
                    log::warn!(
                        "export finished with {} missing file(s) ({}): {}",
                        missing.len(),
                        missing.join(", "),
                        dest.display()
                    );
                    self.raise_transient_status(
                        format!("Bundle exported with {} missing file(s)", missing.len()),
                        now,
                    );
                }
                Err((dest, err)) => {
                    log::warn!("export to {} failed: {err}", dest.display());
                    self.raise_transient_status(format!("Export failed: {err}"), now);
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
        self.poll_update_check(ui.ctx());
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
        // Read before the `self.icons` borrow below, which holds `&mut self`
        // for the rest of the frame.
        let opacity = self.opacity();
        // Issue #340: last frame's measured header band, and the slot this
        // frame's measurement lands in — both locals for the same reason,
        // `self` being borrowed for the whole panel closure below.
        let previous_header_rect = self.header_rect;
        let mut measured_header_rect: Option<egui::Rect> = None;
        let icons = self.icons.get_or_insert_with(|| Icons::load(&ctx));

        // Issue #16 (D1): set by `draw_rows` (via `draw_row`) when a row is
        // right-clicked this frame; consumed below, after this frame's root
        // window rect is known, to open (or re-show) that player's
        // breakdown window.
        let mut opened_skill_uid: Option<i64> = None;
        // Issue #39: the open historical fight — its id, header text and
        // rebuilt `Snapshot` — read once per frame *before* the panel body,
        // since cloning `self.view` for the rest of the frame is what lets
        // the panel closure below still take `&mut self.settings` for
        // `draw_header` without the borrow checker seeing that as aliasing
        // the same historical data. `None` in the `Live` case, and whenever
        // the history view is open but nothing has been loaded yet.
        //
        // Issue #350: `HistoryUi::open` is an `Arc<OpenEncounter>`, so this
        // is a refcount bump, not a deep clone of the held `Snapshot` — and
        // `state.open` is only ever reassigned on a `Loaded`/`Missing`
        // reply or a "← Back"/"← Live" click (see its own doc comment), so
        // consecutive frames between those events hand out the exact same
        // `Arc` allocation.
        let history_open: Option<Arc<OpenEncounter>> = match &self.view {
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
                    .fill(opacity.apply(PANEL_FILL))
                    .stroke(egui::Stroke::new(
                        PANEL_BORDER_WIDTH,
                        opacity.apply(PANEL_BORDER_COLOR),
                    ))
                    .corner_radius(egui::CornerRadius::same(PANEL_CORNER_RADIUS)),
            )
            .show(ui, |ui| {
                // First, so the header buttons drawn afterwards stay on top of
                // the corner zones they overlap.
                let resize_double_clicked =
                    draw_resize_handles(ui, &ctx, &mut self.window_gesture, "root");
                // Issue #300: a resize-border double-click snaps the
                // window's height to whichever 5-row/20-row preset it
                // isn't already at, leaving width untouched — same
                // `InnerSize` command the header dropdown's "Reset to
                // defaults" item already uses for its own (width-and-
                // height) resize.
                let viewport_rect = ctx.input(|i| i.viewport_rect());
                if let Some(target_height) = resize_double_click_command(
                    resize_double_clicked,
                    viewport_rect.height(),
                    measured_header_band_height(previous_header_rect),
                ) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                        viewport_rect.width(),
                        target_height,
                    )));
                }
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
                    previous_header_rect,
                    capturing,
                    share_active,
                    &mut self.update_check,
                    &self.tx_log_export,
                    self.history.is_some(),
                    &mut open_history_clicked,
                    header_history,
                    &mut self.quit_requested,
                );
                // Issue #340: the header's real extent, measured the one place
                // it can be — right after it painted, before anything else
                // has been added to this `Ui`, so `min_rect` is the header
                // band and nothing more (`draw_resize_handles` above only
                // `interact`s, it allocates no space). Stashed on `self`
                // below for the *next* frame's sizing math, the earliest a
                // measurement can reach the code that needs it.
                measured_header_rect = Some(ui.min_rect());
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
                // Issue #253: the user's own artwork behind whichever of
                // the two row surfaces paints next. Deliberately here and
                // not inside `draw_rows`/`draw_history`: painting first is
                // what puts it *behind* every row, hover fill and share
                // bar, and it keeps both of their signatures (and every
                // test that calls them) untouched. `ui.max_rect()` is the
                // central panel's own rect — the same `panel` `draw_header`
                // captured for the header wash — while
                // `available_rect_before_wrap` is the strip left under the
                // header, so `row_backdrop_rect` can clip the image to the
                // rows' area without letting it reach the panel's rounded
                // border.
                draw_row_backdrop(
                    ui,
                    ui.max_rect(),
                    ui.available_rect_before_wrap(),
                    icons,
                    &self.settings,
                );
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
                    tabs: SkillTabs::default(),
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
        // Issue #340: the panel closure has released its borrow of `self`,
        // so this frame's header measurement can finally be recorded. Kept
        // from the previous frame if the header did not paint at all.
        self.header_rect = measured_header_rect.or(self.header_rect);
        let icons = self.icons.as_ref().expect("loaded on the first frame");
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
                        &mut state.tabs,
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

        // PR #268 review, finding 2: tell the pipeline thread which players
        // it can skip the heals/dealt/received/casts breakdowns for. Only
        // `Live`-sourced windows count — a `History`-sourced one draws from
        // `history_open`'s own already-fully-populated snapshot above, never
        // from the live one the pipeline thread builds, so naming its uid
        // here would cost real work for nothing. Sent only when the set
        // actually changed, so an open (or already-focused) window doesn't
        // resend it every frame.
        let live_skill_focus: Vec<i64> = self
            .skill_windows
            .iter()
            .filter(|(_, state)| state.source == SkillWindowSource::Live)
            .map(|(&uid, _)| uid)
            .collect();
        if live_skill_focus != self.last_sent_skill_focus {
            let _ = self
                .tx_command
                .try_send(UiCommand::SkillFocus(live_skill_focus.clone()));
            self.last_sent_skill_focus = live_skill_focus;
        }

        // Issue #349: gated on actual activity rather than an unconditional
        // ~10 Hz — see `repaint::repaint_policy`'s doc comment for the
        // decision table and why each input is gathered here rather than
        // inside the pure function itself. The pipeline thread now wakes
        // the overlay itself the instant it publishes a changed snapshot
        // (`pipeline::RepaintHandle`), so this only needs to catch the case
        // where one is still sitting in the channel un-drained.
        let snapshot_activity = !self.rx_snapshot.is_empty();
        let gif_next_wakeup = icons.custom.borrow().next_wakeup(&ctx);
        let input_active =
            ctx.input(|i| i.pointer.any_down() || i.pointer.is_moving() || !i.events.is_empty());
        // Issue #349: the same three background-thread requests
        // `poll_update_check`/`poll_history` exist to drain — none of them
        // wake the overlay on their own, so the repaint clock is the only
        // thing that ever notices their reply landed.
        let transient_timer_active = self.status_expires_at.is_some()
            || matches!(
                self.update_check,
                UpdateCheckState::Checking { .. } | UpdateCheckState::Installing { .. }
            )
            || matches!(
                &self.view,
                OverlayView::History(state) if state.pending || state.pending_load_id.is_some()
            );
        if let Some(delay) = repaint_policy(RepaintInputs {
            snapshot_activity,
            gif_next_wakeup,
            input_active,
            transient_timer_active,
        }) {
            ctx.request_repaint_after(delay);
        }
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
pub(crate) struct SettingsHandle<'a> {
    pub(crate) settings: &'a mut Settings,
    pub(crate) tx_settings: &'a Sender<Settings>,
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
/// `HWND` cached at startup, because this module's call sites only ever hold
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
///
/// Returns whether any of the eight zones was double-clicked this frame
/// (issue #300) — the root call site turns that into a height-preset
/// resize via `resize_double_click_command`; the breakdown-viewport call
/// site (a skill window has no row-count preset of its own) just discards
/// it. Sensed here rather than left to the caller because only this
/// function actually owns the eight zone `Response`s.
fn draw_resize_handles(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    gesture: &mut WindowGesture,
    // `Debug` alongside `Hash` because egui's `Id::with` takes an
    // `AsIdSalt`, and that is `Hash + Debug` — a debug build records the
    // salt's `Debug` rendering in `id_source` so an id clash names the
    // widgets that collided. Not ours to drop.
    id_salt: impl std::hash::Hash + std::fmt::Debug,
) -> bool {
    // The viewport this `Ui` belongs to — the root window, or, inside
    // `show_viewport_immediate`'s callback, the child. Either way it is the
    // rect `Ui::max_rect` was built from (egui's `root_ui`).
    let window = ctx.input(|i| i.viewport_rect());
    let mut double_clicked = false;
    // `ResizeDirection` is not `Hash`, so the zone's position in the array is
    // what keeps the eight ids distinct.
    for (index, (zone, direction, cursor)) in resize_zones(window).into_iter().enumerate() {
        // `click_and_drag` (rather than plain `drag`) so a double-click on
        // the handle registers as a click pair, not just the first click's
        // drag start — issue #300 needs both out of the same `Response`.
        let handle = ui.interact(
            zone,
            ui.id().with((&id_salt, "resize", index)),
            egui::Sense::click_and_drag(),
        );
        if handle.hovered() {
            ctx.set_cursor_icon(cursor);
        }
        // Same as the title-bar drag: the anchor is captured once, then
        // `drive_window_gesture` does the per-frame work.
        if handle.drag_started_by(egui::PointerButton::Primary) {
            begin_window_gesture(ctx, gesture, GestureKind::Resize(direction));
        }
        if handle.double_clicked_by(egui::PointerButton::Primary) {
            double_clicked = true;
        }
    }
    double_clicked
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
///
/// `previous_header_rect` is the last frame's measured header band
/// (`OverlayApp::header_rect`), or `None` before the first frame — threaded
/// through `measured_header_band_height` so this agrees with the resize-
/// border double-click and the header's own paint (issue #340), rather than
/// re-deriving the band from `header_band_height(BUTTON_ROW_HEIGHT)`'s
/// constant budget alone.
fn default_inner_height(previous_header_rect: Option<egui::Rect>) -> f32 {
    inner_height_for_rows(
        DEFAULT_VISIBLE_ROWS,
        measured_header_band_height(previous_header_rect),
    )
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
///
/// `previous_header_rect` is threaded through to `measured_header_band_
/// height` the same way `default_inner_height` does (issue #340) — see
/// that function's doc comment.
fn reset_to_defaults_inner_height(previous_header_rect: Option<egui::Rect>) -> f32 {
    inner_height_for_rows(
        RESET_TO_DEFAULTS_VISIBLE_ROWS,
        measured_header_band_height(previous_header_rect),
    )
}

/// Shared formula behind both `default_inner_height` and
/// `reset_to_defaults_inner_height` (issue #203 review finding): the header
/// band + separator + gap above the roster, plus `rows` player rows below
/// it. Pulling this out means the two callers can never drift from each
/// other by editing the top-level math in one and not the other — only the
/// row count differs between them, and that lives in their own constants.
fn inner_height_for_rows(rows: usize, band_height: f32) -> f32 {
    let rows = rows as f32 * ROW_HEIGHT;
    first_player_row_top_offset(band_height) + rows
}

/// Issue #300: the inner height a resize-border double-click should snap
/// the window to, given its inner height right now — alternating between
/// the same two presets `reset_to_defaults_inner_height`
/// (`RESET_TO_DEFAULTS_VISIBLE_ROWS`, 5) and `default_inner_height`
/// (`DEFAULT_VISIBLE_ROWS`, 20) already compute.
///
/// No latched "which preset did the last double-click apply" state is
/// kept anywhere — the current height alone decides: whichever preset is
/// farther from it wins, with the midpoint between the two as the tie
/// line. That is what makes back-to-back double-clicks alternate at all:
/// landing on one preset puts the window closer to it and farther from
/// the other, so the very next double-click's farther-preset pick is
/// always the other one. It also means a window resized by hand to some
/// arbitrary height resolves its first double-click sensibly, with
/// nothing to initialize.
fn resize_double_click_preset_height(current_height: f32, band_height: f32) -> f32 {
    let five_rows = inner_height_for_rows(RESET_TO_DEFAULTS_VISIBLE_ROWS, band_height);
    let twenty_rows = inner_height_for_rows(DEFAULT_VISIBLE_ROWS, band_height);
    let midpoint = (five_rows + twenty_rows) / 2.0;
    if current_height < midpoint {
        twenty_rows
    } else {
        five_rows
    }
}

/// Issue #300: turns "did a resize-border zone get double-clicked this
/// frame" (`draw_resize_handles`' own return value) into the `InnerSize`
/// height command the root call site should queue, if any.
///
/// Kept separate from `resize_double_click_preset_height` so the "was
/// there actually a double-click this frame" gate and the "what height
/// does that resolve to" math stay two independently testable decisions —
/// without it, every ordinary frame (no double-click at all) would need
/// its own `current_height` threaded through the preset math just to
/// throw the answer away.
fn resize_double_click_command(
    double_clicked: bool,
    current_height: f32,
    band_height: f32,
) -> Option<f32> {
    double_clicked.then(|| resize_double_click_preset_height(current_height, band_height))
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
        .unwrap_or([default_inner_width(), default_inner_height(None)]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ColumnKind, Settings};
    use bpsr_meter::Class;

    // -- Skill-icon monogram placeholder colors (issue #275) ----------------

    #[test]
    fn placeholder_colors_are_deterministic_per_id() {
        for id in [2031103, 2203291, 35107, -5, 0] {
            assert_eq!(skill_placeholder_colors(id), skill_placeholder_colors(id));
        }
    }

    #[test]
    fn different_ids_sharing_a_monogram_can_still_land_on_different_swatches() {
        // Every Lucky Strike weapon variant collapses to the same "LS"
        // glyph (they are literally the same base skill), so the id-keyed
        // background is the only thing that can still tell the rows apart.
        let ids = [2031101, 2031102, 2031103, 2031105];
        let colors: std::collections::HashSet<_> = ids
            .iter()
            .map(|&id| {
                let (bg, _fg) = skill_placeholder_colors(id);
                (bg.r(), bg.g(), bg.b())
            })
            .collect();
        assert!(
            colors.len() > 1,
            "expected the four Lucky Strike variants to spread across more than one swatch"
        );
    }

    #[test]
    fn every_placeholder_swatch_clears_wcag_normal_text_contrast_with_its_chosen_glyph_color() {
        // Exercises the legibility rule this placeholder encodes: whichever
        // of near-black or white `skill_placeholder_colors` picks must clear
        // WCAG AA's 4.5:1 minimum for normal text against every swatch in
        // the palette, not just the ones a spot check happens to hit.
        // Issue #281's live-window pass found two swatches (teal, moss
        // green) that cleared the looser 3:1 large-text floor but fell
        // short of 4.5 — this asserts the stricter bound so that class of
        // regression can't land unnoticed again.
        for &(r, g, b) in &SKILL_PLACEHOLDER_PALETTE {
            let bg_lum = relative_luminance((r, g, b));
            let fg = if bg_lum > 0.2017 {
                (0x1A, 0x1A, 0x1A)
            } else {
                (0xFF, 0xFF, 0xFF)
            };
            let fg_lum = relative_luminance(fg);
            let (hi, lo) = if bg_lum > fg_lum {
                (bg_lum, fg_lum)
            } else {
                (fg_lum, bg_lum)
            };
            let ratio = (hi + 0.05) / (lo + 0.05);
            assert!(
                ratio >= 4.5,
                "swatch {:?} only reaches a {ratio:.2}:1 contrast ratio",
                (r, g, b)
            );
        }
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

    // -- heals/dealt/received/casts demo fixtures (PR #268 review, finding
    // 3) --------------------------------------------------------------
    //
    // Every test above this point only ever looks at `row.skills`; nothing
    // in this file asserted the other four tabs `demo_snapshot` claims to
    // populate (issue #245) actually are. These mirror the `skills` tests'
    // shape for each of `heals`/`dealt`/`received`/`casts`.

    /// Only the demo party's healer class (`VerdantOracle`/`Fizz`) has any
    /// healing fixture (`demo_heals`) — every other row must come through
    /// with an empty `heals`, and the healer's own must be non-empty and
    /// sum to `heal_total`'s declared denominator, its shares summing to
    /// ~100% the same way `demo_snapshot_header_and_rows_are_internally_
    /// consistent` checks the damage rows.
    #[test]
    fn demo_heal_breakdown_is_populated_only_for_the_healer_role_and_sums_correctly() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            let class = row.class.expect("every demo row has a class");
            let total = heal_total(class);
            if total == 0 {
                assert!(
                    row.heals.is_empty(),
                    "row {} ({class:?}) has no heal fixture and must have an empty heals tab",
                    row.name
                );
                continue;
            }
            assert!(
                !row.heals.is_empty(),
                "row {} ({class:?}) must have a non-empty heals tab",
                row.name
            );
            let heal_sum: i64 = row.heals.iter().map(|s| s.damage).sum();
            assert_eq!(
                heal_sum, total,
                "row {}'s heal breakdown must sum to heal_total",
                row.name
            );
            let share_sum: f32 = row.heals.iter().map(|s| s.share_pct).sum();
            assert!(
                (share_sum - 100.0).abs() < 0.1,
                "row {}'s heal shares must sum to ~100%, got {share_sum}",
                row.name
            );
        }
    }

    /// The boss's swings (`DEMO_RECEIVED`) land on every row identically —
    /// there is no per-class variation the way heals has — so every row's
    /// `received` must be non-empty and sum to `demo_received_total`.
    #[test]
    fn demo_received_breakdown_is_populated_for_every_row_and_sums_to_its_total() {
        let snapshot = demo_snapshot();
        let total = demo_received_total();
        for row in &snapshot.rows {
            assert!(
                !row.received.is_empty(),
                "row {} must have a non-empty received tab",
                row.name
            );
            let received_sum: i64 = row.received.iter().map(|s| s.damage).sum();
            assert_eq!(
                received_sum, total,
                "row {}'s received breakdown must sum to demo_received_total",
                row.name
            );
        }
    }

    /// `demo_cast_rows` is built 1:1 from `demo_skills` (issue #245): same
    /// skill ids, hits floored at one third of the damage row's own hit
    /// count. Every row's casts must be non-empty and match that exactly —
    /// this is the "populated" half of finding 3's coverage gap for the
    /// Skill casts tab.
    #[test]
    fn demo_cast_breakdown_is_populated_and_derives_from_the_skill_hits() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            assert!(
                !row.casts.is_empty(),
                "row {} must have a non-empty casts tab",
                row.name
            );
            assert_eq!(
                row.casts.len(),
                row.skills.len(),
                "row {}'s casts must have one entry per skill",
                row.name
            );
            for skill in &row.skills {
                let cast = row
                    .casts
                    .iter()
                    .find(|c| c.skill_id == skill.skill_id)
                    .unwrap_or_else(|| {
                        panic!(
                            "row {}'s casts is missing skill id {}",
                            row.name, skill.skill_id
                        )
                    });
                assert_eq!(
                    cast.hits,
                    (skill.hits / 3).max(1),
                    "row {}'s cast count for skill {} must be its hit count / 3, floored at 1",
                    row.name,
                    skill.skill_id
                );
            }
        }
    }

    /// The "Skill dealt" tab is damage and healing merged under one skill
    /// id (issue #245) — for the current, non-colliding fixtures that's
    /// equivalent to a plain concatenation, so this only pins the *sum*:
    /// `demo_dealt_rows_merges_a_shared_skill_id_instead_of_duplicating_it`
    /// below is what actually exercises the merge itself.
    #[test]
    fn demo_dealt_breakdown_sums_to_damage_plus_heals() {
        let snapshot = demo_snapshot();
        for row in &snapshot.rows {
            let heal_sum: i64 = row.heals.iter().map(|s| s.damage).sum();
            let dealt_sum: i64 = row.dealt.iter().map(|s| s.damage).sum();
            assert_eq!(
                dealt_sum,
                row.damage + heal_sum,
                "row {}'s dealt breakdown must sum to damage + heals",
                row.name
            );
            assert!(
                !row.dealt.is_empty(),
                "row {} must have a non-empty dealt tab",
                row.name
            );
        }
    }

    /// PR #268 review, finding 3: the "Skill dealt" tab used to be built by
    /// chaining `demo_skill_rows(demo_skills, ...)` and
    /// `demo_skill_rows(demo_heals(class), ...)` end to end, with no merge
    /// step — so a skill id that happened to appear in *both* fixture lists
    /// would silently show up as two separate rows instead of one summed
    /// row, unlike the real meter's `dealt_rows`
    /// (`crates/meter/src/encounter.rs`), which merges by skill id via
    /// `SkillStats::merge`. None of today's `DEMO_ROWS` fixtures happen to
    /// collide, so that bug shipped invisibly; this drives `demo_dealt_rows`
    /// directly with two synthetic lists sharing a skill id and would fail
    /// against the old chain-based implementation (two rows, wrong per-row
    /// damage) the same way it would against a real collision.
    #[test]
    fn demo_dealt_rows_merges_a_shared_skill_id_instead_of_duplicating_it() {
        let skills: &[DemoSkill] = &[
            (9001, 1_000, 10, 2, 400, 200), // shared id
            (9002, 500, 5, 1, 200, 150),
        ];
        let heals: &[DemoSkill] = &[
            (9001, 300, 3, 1, 150, 100), // same id as skills[0]
        ];
        let rows = demo_dealt_rows(skills, heals, 1_800, 60_000);

        assert_eq!(
            rows.iter().filter(|r| r.skill_id == 9001).count(),
            1,
            "a skill id shared between skills and heals must merge into one dealt row, not two"
        );
        let merged = rows
            .iter()
            .find(|r| r.skill_id == 9001)
            .expect("the merged 9001 row must be present");
        assert_eq!(
            merged.damage, 1_300,
            "the merged row's damage must be the sum of both sources' damage"
        );
        assert_eq!(
            merged.hits, 13,
            "the merged row's hits must be the sum of both sources' hits"
        );

        let untouched = rows
            .iter()
            .find(|r| r.skill_id == 9002)
            .expect("a non-colliding skill id must still come through");
        assert_eq!(untouched.damage, 500);
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

    /// Issue #321: `run`'s own break on `UiCommand::Quit` (or a disconnect
    /// of its command channel) drops `tx_snapshot` right away, so an
    /// orderly quit hits this exact `TryRecvError::Disconnected` arm one
    /// egui frame later — the same arm issue #214's real failure mode (the
    /// pipeline thread dying mid-session) raises the permanent "frozen"
    /// banner from. `quit_requested`, set at both `UiCommand::Quit` send
    /// sites, is what tells the two apart: a disconnect seen after it must
    /// leave the status alone rather than raising that banner.
    #[test]
    fn drain_snapshots_disconnected_after_quit_does_not_raise_the_dead_pipeline_status() {
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
        app.quit_requested = true;

        drop(tx_snapshot);
        app.drain_snapshots();

        assert_eq!(
            app.status,
            StatusLine::Ok,
            "a disconnect after Quit is an orderly shutdown, not a dead pipeline"
        );
        assert_eq!(
            app.status_expires_at, None,
            "no banner means no expiry either"
        );
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

        app.tx_log_export
            .send(Ok((dest.clone(), Vec::new())))
            .unwrap();
        app.poll_log_export(now);
        assert_eq!(
            app.status,
            StatusLine::Ok,
            "an export that worked must not raise a banner"
        );

        // A bundle that landed but came up short: not a failure, but the
        // folder looks identical to a clean one, so it must still say so.
        app.tx_log_export
            .send(Ok((dest.clone(), vec!["dump.jsonl.1".to_owned()])))
            .unwrap();
        app.poll_log_export(now);
        assert!(
            matches!(&app.status, StatusLine::Error(msg) if msg.contains("1 missing file")),
            "an export missing a file must say how many on the banner: {:?}",
            app.status
        );
        app.expire_transient_status(now + TRANSIENT_STATUS_LINGER);

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
    pub(crate) fn header_test_snapshot(total_damage: i64) -> Snapshot {
        Snapshot {
            duration_ms: 90_000,
            total_damage,
            total_dps: 12_345.0,
            total_absorbed: 0,
            total_immune: 0,
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
            local_uid: None,
            capture_alive: true,
        }
    }

    /// Walks a painted `Shape`, collecting the text of every `Shape::Text`
    /// found — recursing into `Shape::Vec` since egui groups a layout's
    /// child shapes (e.g. `ui.horizontal`'s row) that way. `Galley`
    /// dereferences to `str` (`Deref<Target = str>`), so `galley.text()`
    /// hands back exactly the string that was laid out.
    pub(crate) fn collect_text_shapes(shape: &egui::Shape, out: &mut Vec<String>) {
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
    pub(crate) fn collect_image_texture_tints(
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
    pub(crate) fn collect_circle_fills(shape: &egui::Shape, out: &mut Vec<egui::Color32>) {
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

    /// Walks a painted `Shape`, collecting every `Shape::Circle`'s
    /// `(center, radius)` — like `collect_circle_fills` but keeping the
    /// geometry instead of the fill, for a caller that needs to check
    /// *where* and *how big* a circle painted, not just what color.
    pub(crate) fn collect_circle_geometry(shape: &egui::Shape, out: &mut Vec<(egui::Pos2, f32)>) {
        match shape {
            egui::Shape::Circle(circle) => out.push((circle.center, circle.radius)),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_circle_geometry(s, out);
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
    pub(crate) fn header_rendered_texts(snapshot: &Snapshot) -> Vec<String> {
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
                None,
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
                &mut false,
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
    pub(crate) struct HeaderFrame {
        pub(crate) texts: Vec<(String, egui::Rect)>,
        pub(crate) images: Vec<(egui::TextureId, egui::Rect)>,
        pub(crate) rects: Vec<(egui::Color32, egui::Rect)>,
        pub(crate) glyphs: Vec<(GlyphIcon, egui::TextureId)>,
    }

    impl HeaderFrame {
        /// The union of every text shape painted for `value` — plural
        /// because the faux-bold pass paints the same string twice, and both
        /// passes together are what the eye sees.
        pub(crate) fn text_box(&self, value: &str) -> egui::Rect {
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
        pub(crate) fn glyph_boxes(&self, glyph: GlyphIcon) -> Vec<egui::Rect> {
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
        pub(crate) fn gradient_box(&self) -> egui::Rect {
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
        pub(crate) fn fill_box(&self, fill: egui::Color32) -> egui::Rect {
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
    pub(crate) fn collect_painted_boxes(
        shape: &egui::Shape,
        clip: egui::Rect,
        frame: &mut HeaderFrame,
    ) {
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
    pub(crate) fn header_painted_boxes(snapshot: &Snapshot) -> HeaderFrame {
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
                egui::vec2(default_inner_width(), default_inner_height(None)),
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
                None,
                false,
                true,
                &mut UpdateCheckState::default(),
                &unused_log_export_sender(),
                false,
                &mut false,
                None,
                &mut false,
            );
        });
        for clipped in &output.shapes {
            collect_painted_boxes(&clipped.shape, clipped.clip_rect, &mut frame);
        }
        output.drop_without_applying_deltas();
        frame
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

    /// The title row `title_row_toggles` is drawn into, for a test that
    /// calls it directly rather than through `draw_header`: the top
    /// `TITLE_LINE_HEIGHT` of whatever space the test's `Ui` has, which is
    /// exactly the rect `draw_title_line` allocates and hands over in the
    /// real call path.
    pub(crate) fn test_title_row(ui: &egui::Ui) -> egui::Rect {
        egui::Rect::from_min_size(
            ui.available_rect_before_wrap().min,
            egui::vec2(ui.available_width(), TITLE_LINE_HEIGHT),
        )
    }

    pub(crate) fn crop_test_image(width: usize, height: usize) -> std::sync::Arc<egui::ColorImage> {
        let pixels = (0..width * height)
            .map(|i| egui::Color32::from_gray((i % 256) as u8))
            .collect();
        std::sync::Arc::new(egui::ColorImage::new([width, height], pixels))
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

    pub(crate) fn sample_row(ability_score: Option<u32>) -> PlayerRow {
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
            dead_ms: Some(0),
            ability_score,
            season_strength: None,
            imagines: [None, None],
            imagine_tiers: [None, None],
            skills: Vec::new(),
            heals: Vec::new(),
            dealt: Vec::new(),
            received: Vec::new(),
            casts: Vec::new(),
            buffs: Vec::new(),
            absorbed_total: 0,
            immune_total: 0,
            shield: None,
        }
    }

    pub(crate) fn sample_season_row(season_strength: Option<u32>) -> PlayerRow {
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

    pub(crate) fn sample_score_row(
        ability_score: Option<u32>,
        season_strength: Option<u32>,
    ) -> PlayerRow {
        PlayerRow {
            season_strength,
            ..sample_row(ability_score)
        }
    }

    pub(crate) fn window() -> egui::Rect {
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

    // -- header background wash (issue #59, #62, #81) --------------------

    /// A stand-in central-panel rect for the wash geometry tests — wider and
    /// far taller than the wash itself, like the real panel.
    pub(crate) fn wash_test_panel() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(12.0, 30.0), egui::vec2(400.0, 300.0))
    }

    // -- custom background images (issues #121, #253) ---------------------

    /// #253's hard requirement, and #121's by extension: the image is
    /// painted at `.gamma_multiply(settings.opacity)`, so the existing
    /// slider fades it exactly as it fades `PANEL_FILL`. A backdrop that
    /// painted at a fixed opacity regardless of the slider is precisely the
    /// defect the issue exists to prevent, so the tint's dependence on the
    /// slider is asserted at both endpoints and in between rather than
    /// assumed.
    #[test]
    fn background_image_tint_tracks_the_opacity_slider() {
        assert_eq!(
            background_image_tint(Opacity::OPAQUE),
            egui::Color32::WHITE,
            "a full-opacity overlay must paint the image untouched"
        );
        assert_eq!(
            background_image_tint(Opacity::new(Settings::OPACITY_MIN)).a(),
            0,
            "a zero-opacity overlay must paint no image at all"
        );
        let half = background_image_tint(Opacity::new(0.5)).a();
        assert!(
            half > 0 && half < 255,
            "a mid-slider overlay must paint a partly-faded image, got alpha {half}"
        );
        assert!(
            background_image_tint(Opacity::new(0.25)).a() < half,
            "dragging the slider down must fade the image further"
        );
    }

    /// The scrim fades with the image rather than staying put, so the
    /// contrast guarantee it provides holds at every slider position — and
    /// so a 0% overlay is genuinely empty rather than showing a dark
    /// rectangle where the artwork was.
    #[test]
    fn background_image_scrim_tracks_the_opacity_slider() {
        assert_eq!(
            BACKGROUND_IMAGE_SCRIM.gamma_multiply(1.0),
            BACKGROUND_IMAGE_SCRIM
        );
        assert_eq!(
            BACKGROUND_IMAGE_SCRIM
                .gamma_multiply(Settings::OPACITY_MIN)
                .a(),
            0
        );
        assert!(
            BACKGROUND_IMAGE_SCRIM.gamma_multiply(0.5).a() < BACKGROUND_IMAGE_SCRIM.a(),
            "the scrim must fade along with the image it dims"
        );
    }

    /// The scrim exists to pull a bright image back towards the panel's own
    /// tone, so it has to *be* that tone — and it has to be partial, since a
    /// fully opaque scrim would hide the artwork entirely and a fully
    /// transparent one would guarantee nothing.
    #[test]
    fn background_image_scrim_matches_the_panel_fill_color() {
        // Unmultiplied on both sides: `Color32` stores premultiplied
        // channels, so the scrim's raw `.r()` is already scaled by its own
        // alpha and would never equal the opaque `PANEL_FILL`'s.
        let scrim = BACKGROUND_IMAGE_SCRIM.to_srgba_unmultiplied();
        assert_eq!(
            [scrim[0], scrim[1], scrim[2]],
            [PANEL_FILL.r(), PANEL_FILL.g(), PANEL_FILL.b()],
        );
        assert_eq!(scrim[3], BACKGROUND_IMAGE_SCRIM_ALPHA);
        assert!(
            (1..255).contains(&BACKGROUND_IMAGE_SCRIM_ALPHA),
            "the scrim must dim the image without hiding it"
        );
    }

    /// A stand-in wash height for the geometry tests below — issue #81 made
    /// this the caller's to choose (`draw_header` derives it from
    /// `header_text_band_height`) rather than a fixed constant, so these
    /// tests exercise the geometry with an arbitrary value of their own.
    pub(crate) const WASH_TEST_HEIGHT: f32 = 34.0;

    // -- column_anchors (issue #8) --------------------------------------

    /// A stand-in three-column layout (same widths the old fixed
    /// `STAT_COLUMNS` array used) for tests that exercise `column_anchors`'
    /// pure math and don't care where the widths came from.
    pub(crate) const TEST_COLUMNS: [StatColumn; 3] = [
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

    // -- icon slot geometry (issue #9, issue #33) --------------------------

    pub(crate) fn row_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(300.0, ROW_HEIGHT))
    }

    // -- damage-share bar paints (issue #43) --------------------------------

    pub(crate) fn share_bar_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(300.0, ROW_HEIGHT))
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

    // -- breakdown-window chrome gestures (issue #218) ----------------------

    /// The window rect these gesture tests measure against — off-origin so
    /// an accidental `0.0` in the maths cannot pass by coincidence.
    pub(crate) fn skill_window_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(120.0, 80.0), egui::vec2(880.0, 520.0))
    }

    /// Collects the painted (and clip-intersected) rect of every `Text`
    /// shape whose galley text is exactly `name` — the same clip-aware
    /// extraction `collect_painted_boxes` does for the main header's tests,
    /// scoped down to just the one string this test cares about.
    pub(crate) fn collect_name_text_boxes(
        shape: &egui::Shape,
        clip: egui::Rect,
        name: &str,
        out: &mut Option<egui::Rect>,
    ) {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == name => {
                let rect = egui::Rect::from_min_size(text.pos, text.galley.size()).intersect(clip);
                *out = Some(out.map_or(rect, |existing| existing.union(rect)));
            }
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_name_text_boxes(s, clip, name, out);
                }
            }
            _ => {}
        }
    }

    /// Every `Shape::Rect` a frame painted, flattened out of the `Vec`
    /// nesting -- `collect_row_boxes` deliberately keeps only text and
    /// meshes, and a scrollbar is neither.
    pub(crate) fn painted_rects(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
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
    pub(crate) fn assert_bar_hue(class: Option<Class>, expected_rgb: (u8, u8, u8)) {
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

    #[test]
    fn default_size_stays_above_the_min_inner_size() {
        // `with_min_inner_size` is [220.0, 90.0] (unaffected by issue #26);
        // the default opening size must never start below its own floor.
        assert!(default_inner_width() >= 220.0);
        assert!(default_inner_height(None) >= 90.0);
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
            default_inner_height(None) - reset_to_defaults_inner_height(None),
            row_delta * ROW_HEIGHT
        );
    }

    /// Issue #340: the presets are built from the header band the overlay
    /// has actually painted, not from the constant budget — a header that
    /// measured taller pushes both snap targets down by exactly that much,
    /// so a double-click still lands the requested row count instead of
    /// clipping the last row.
    #[test]
    fn resize_double_click_presets_track_the_measured_header_band() {
        let budget = header_band_height(BUTTON_ROW_HEIGHT);
        let taller = budget + 12.0;
        let five = inner_height_for_rows(RESET_TO_DEFAULTS_VISIBLE_ROWS, taller);
        let twenty = inner_height_for_rows(DEFAULT_VISIBLE_ROWS, taller);
        assert_eq!(five, reset_to_defaults_inner_height(None) + 12.0);
        assert_eq!(twenty, default_inner_height(None) + 12.0);
        assert_eq!(resize_double_click_preset_height(five, taller), twenty);
        assert_eq!(resize_double_click_preset_height(twenty, taller), five);
    }

    /// The gate stays a separate decision from the math, and passes the
    /// measured band straight through.
    #[test]
    fn resize_double_click_command_uses_the_measured_band_when_it_fires() {
        let band = header_band_height(BUTTON_ROW_HEIGHT) + 5.0;
        assert_eq!(resize_double_click_command(false, 400.0, band), None);
        assert_eq!(
            resize_double_click_command(true, 400.0, band),
            Some(resize_double_click_preset_height(400.0, band))
        );
    }

    /// Issue #300: double-clicking a resize border snaps the window
    /// straight to whichever of the two presets it isn't already at — so a
    /// window already sitting exactly on one preset always flips to the
    /// other on the next double-click, the "alternating" the issue asks
    /// for.
    #[test]
    fn resize_double_click_preset_height_alternates_between_the_two_presets() {
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        let five = reset_to_defaults_inner_height(None);
        let twenty = default_inner_height(None);
        assert_eq!(resize_double_click_preset_height(five, band), twenty);
        assert_eq!(resize_double_click_preset_height(twenty, band), five);
    }

    /// Issue #340: `default_inner_height` and `reset_to_defaults_inner_
    /// height` both thread the same measured header rect into `measured_
    /// header_band_height`, and the double-click preset swap sizes its own
    /// band the identical way — so given one real measured rect (not the
    /// constant budget), the reset preset must still land exactly on the
    /// height double-clicking the resize border would produce for that
    /// same rect, and vice versa. If any of the three ever fell back to
    /// re-deriving the band from `header_band_height(BUTTON_ROW_HEIGHT)`
    /// instead of the measured rect, this would catch the resulting drift.
    #[test]
    fn reset_to_defaults_and_double_click_agree_on_height_for_a_measured_rect() {
        let measured = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 81.0));
        let band = measured_header_band_height(Some(measured));
        let five = reset_to_defaults_inner_height(Some(measured));
        let twenty = default_inner_height(Some(measured));
        assert_eq!(resize_double_click_preset_height(five, band), twenty);
        assert_eq!(resize_double_click_preset_height(twenty, band), five);
    }

    /// From any height that isn't already sitting on a preset (a window
    /// resized by hand, or one that has never been snapped), the target is
    /// whichever preset is farther away — the midpoint between the two
    /// presets is where that flips.
    #[test]
    fn resize_double_click_preset_height_picks_the_farther_preset_from_an_arbitrary_height() {
        let five = reset_to_defaults_inner_height(None);
        let twenty = default_inner_height(None);
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        let midpoint = (five + twenty) / 2.0;
        assert_eq!(
            resize_double_click_preset_height(midpoint - 1.0, band),
            twenty
        );
        assert_eq!(
            resize_double_click_preset_height(midpoint + 1.0, band),
            five
        );
    }

    /// A frame with no resize-border double-click this frame must never
    /// queue a resize command, no matter what the current height is —
    /// otherwise every ordinary frame would re-issue the same `InnerSize`
    /// command against whatever height a manual drag last left the window
    /// at.
    #[test]
    fn resize_double_click_command_is_none_without_a_double_click() {
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        assert_eq!(resize_double_click_command(false, 100.0, band), None);
        assert_eq!(
            resize_double_click_command(false, reset_to_defaults_inner_height(None), band),
            None
        );
    }

    /// A resize-border double-click turns into exactly the target height
    /// `resize_double_click_preset_height` computes from the window's
    /// current height.
    #[test]
    fn resize_double_click_command_uses_the_preset_height_when_double_clicked() {
        let band = header_band_height(BUTTON_ROW_HEIGHT);
        let five = reset_to_defaults_inner_height(None);
        let twenty = default_inner_height(None);
        assert_eq!(resize_double_click_command(true, five, band), Some(twenty));
        assert_eq!(resize_double_click_command(true, twenty, band), Some(five));
    }

    /// Every row shares the same damage, so `row_bar_frac` (relative to the
    /// top row) is exactly `1.0` for all of them and `share_bar_paints`'
    /// `fill_rect` therefore equals the *full* row rect for every row — the
    /// only painted shape wide enough to use as each row's ground-truth
    /// rect in `RowFrame::row_rects`.
    pub(crate) fn rows_test_snapshot(n: usize) -> Snapshot {
        Snapshot {
            duration_ms: 90_000,
            total_damage: 1_000 * n as i64,
            total_dps: 12_345.0,
            total_absorbed: 0,
            total_immune: 0,
            rows: (0..n)
                .map(|i| PlayerRow {
                    name: format!("P{i}"),
                    damage: 1_000,
                    ..sample_row(None)
                })
                .collect(),
            encounter: EncounterInfo::default(),
            local_uid: None,
            capture_alive: true,
        }
    }

    /// One `draw_rows` frame's painted text and mesh geometry — mirrors
    /// `HeaderFrame`/`collect_painted_boxes` (issue #75) but for the row
    /// list (issue #83's regression harness): meshes rather than filled
    /// rects, because `draw_row` paints the share bar and hover highlight
    /// as gradient meshes (`Shape::Mesh`), never a flat `Shape::Rect`.
    pub(crate) struct RowFrame {
        /// In paint order, so a test can compare two texts' indices to pin
        /// z-order (egui paints shapes in call order) — that is how the
        /// inline name suffix is proven to paint *before* the stat-column
        /// loop. The `Color32` is the galley's own text color, which is
        /// what makes the suffix's dimming (`NAME_SUFFIX_ALPHA`) checkable
        /// from the painted output rather than from the call site.
        pub(crate) texts: Vec<(String, egui::Rect, egui::Color32)>,
        pub(crate) meshes: Vec<egui::Rect>,
    }

    impl RowFrame {
        /// The union of every text shape painted for `value` (a player
        /// name here).
        pub(crate) fn text_box(&self, value: &str) -> egui::Rect {
            self.texts
                .iter()
                .filter(|(painted, ..)| painted == value)
                .map(|(_, rect, _)| *rect)
                .reduce(egui::Rect::union)
                .unwrap_or_else(|| panic!("draw_rows never painted {value:?}: {:?}", self.texts))
        }

        /// The index in paint order of the first text shape painted for
        /// `value`, plus the color it was painted in.
        pub(crate) fn text_paint(&self, value: &str) -> (usize, egui::Color32) {
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
        pub(crate) fn row_rects(&self) -> Vec<egui::Rect> {
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

    pub(crate) fn collect_row_boxes(shape: &egui::Shape, clip: egui::Rect, frame: &mut RowFrame) {
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
    pub(crate) fn rows_painted_boxes(snapshot: &Snapshot, width: f32, height: f32) -> RowFrame {
        rows_painted_boxes_with(snapshot, &Settings::default(), width, height)
    }

    /// `rows_painted_boxes` with the settings under test spelled out —
    /// needed by anything that has to render a non-default column set
    /// (issue #168's inline `AbilityScore`/`SeasonStrength`, which
    /// `Settings::default` does not enable).
    pub(crate) fn rows_painted_boxes_with(
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
    pub(crate) fn rows_content_size(snapshot: &Snapshot, width: f32, height: f32) -> egui::Vec2 {
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
        let content =
            rows_content_size(&snapshot, default_inner_width(), default_inner_height(None));
        assert!(
            content.x <= default_inner_width() + 0.01,
            "content {content:?} must not exceed the {}pt default width",
            default_inner_width()
        );
        assert!(
            content.y <= default_inner_height(None) + 0.01,
            "content {content:?} must not exceed the {}pt default height",
            default_inner_height(None)
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
    fn each_rows_name_text_is_vertically_centered_in_its_row() {
        let snapshot = rows_test_snapshot(DEFAULT_VISIBLE_ROWS);
        let frame =
            rows_painted_boxes(&snapshot, default_inner_width(), default_inner_height(None));
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

    // -- window position tracking (issue #27) -----------------------------

    /// Calls `track_window_position` with `outer_rect`/`minimized` exactly
    /// as `OverlayApp::update` passes them post issue #134 review (read
    /// once by the caller and shared with `track_window_size`, rather than
    /// each tracker re-reading `ctx.input` itself). Returns everything it
    /// sent on the settings-writer channel.
    pub(crate) fn track_one_frame(
        settings: &mut Settings,
        outer_rect: Option<egui::Rect>,
        minimized: Option<bool>,
    ) -> Vec<Settings> {
        let (tx, rx) = crossbeam_channel::unbounded();
        track_window_position(outer_rect, minimized, settings, &tx);
        drop(tx);
        rx.try_iter().collect()
    }

    pub(crate) fn outer_rect_at(x: f32, y: f32) -> Option<egui::Rect> {
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
    pub(crate) fn track_size_one_frame(
        settings: &mut Settings,
        inner_rect: Option<egui::Rect>,
        minimized: Option<bool>,
    ) -> Vec<Settings> {
        let (tx, rx) = crossbeam_channel::unbounded();
        track_window_size(inner_rect, minimized, settings, &tx);
        drop(tx);
        rx.try_iter().collect()
    }

    pub(crate) fn inner_rect_of(width: f32, height: f32) -> Option<egui::Rect> {
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
            Some(egui::vec2(
                default_inner_width(),
                default_inner_height(None)
            ))
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
            Some(egui::vec2(
                default_inner_width(),
                default_inner_height(None)
            )),
            "a non-finite persisted size must be rejected outright, not clamped"
        );
    }

    #[test]
    fn viewport_falls_back_to_default_size_for_an_absurdly_large_persisted_value() {
        let built = viewport(None, Some([1.0e9, 480.0]));

        assert_eq!(
            built.inner_size,
            Some(egui::vec2(
                default_inner_width(),
                default_inner_height(None)
            )),
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
            Some(egui::vec2(
                default_inner_width(),
                default_inner_height(None)
            )),
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
            Some(egui::vec2(
                default_inner_width(),
                default_inner_height(None)
            ))
        );
    }

    /// Reads back the accessible ("label") name AccessKit would announce for
    /// `id`, out of a full frame's `FullOutput::platform_output::
    /// accesskit_update`. `None`
    /// covers both "no accesskit update at all" and "a node exists but
    /// carries no label" — both mean a screen-reader user hears nothing.
    pub(crate) fn accessible_label(
        update: &egui::accesskit::TreeUpdate,
        id: egui::Id,
    ) -> Option<String> {
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
    pub(crate) fn accessible_rect_for_label(
        update: &egui::accesskit::TreeUpdate,
        label: &str,
    ) -> egui::Rect {
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
    pub(crate) fn accessible_rect_for_role(
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
    pub(crate) fn click_at(pos: egui::Pos2) -> egui::RawInput {
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
    pub(crate) fn press_at(pos: egui::Pos2) -> egui::RawInput {
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
    pub(crate) fn release_at(pos: egui::Pos2) -> egui::RawInput {
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

    /// Renders `draw_header_menu` with `update_check` in `state` and
    /// returns every string it painted. Issue #250 added three more states
    /// to that dropdown, and each of them is a line the user has to be able
    /// to read — asserting on the painted text is the only way to check
    /// that from a headless host.
    pub(crate) fn header_menu_texts(state: UpdateCheckState) -> Vec<String> {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut update_check = state;

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_header_menu(
                ui,
                &ctx,
                &tx_command,
                SettingsHandle {
                    settings: &mut settings,
                    tx_settings: &tx_settings,
                },
                None,
                &icons,
                &mut update_check,
                &unused_log_export_sender(),
                &mut false,
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text_shapes(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();
        texts
    }

    pub(crate) fn update_available(asset_url: Option<&str>) -> CheckOutcome {
        CheckOutcome::UpdateAvailable {
            tag: "v0.3.0".to_string(),
            url: "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0".to_string(),
            asset_url: asset_url.map(str::to_string),
        }
    }

    // --- Manual (app-driven) window move/resize gestures, issue #11 ---

    /// A stand-in window rect, deliberately larger than `MIN_INNER_SIZE` on
    /// both axes and off the origin so a drift in either edge shows up.
    pub(crate) fn window_rect() -> egui::Rect {
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

    /// Walks a painted `Shape`, collecting every `Shape::Mesh`'s texture id
    /// — the counterpart to `collect_text_shapes`, for the one pill glyph
    /// that is blitted rather than stroked.
    pub(crate) fn collect_image_textures(shape: &egui::Shape, out: &mut Vec<egui::TextureId>) {
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
    pub(crate) fn counter_pill_textures(icon: Option<egui::TextureId>) -> Vec<egui::TextureId> {
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

    // --- the chevron itself (issue #54) ----------------------------------

    pub(crate) fn title_row() -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(default_inner_width(), TITLE_LINE_HEIGHT),
        )
    }

    // -- click-through hit box (issue #167 rehash) --------------------------

    /// The cluster rect the header allocates for `toggle_cluster`, at a
    /// round origin so the expected bounds below are readable.
    pub(crate) fn toggle_cluster_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 10.0), egui::vec2(80.0, 20.0))
    }

    // -- per-player skill breakdown window (issue #16) --------------------

    /// A single click (move, press, release, all in one frame) at `pos`
    /// with `button` — `click_at`'s shape, generalized to any button, so a
    /// right-click gesture can be synthesized the same way.
    pub(crate) fn click_at_with_button(
        pos: egui::Pos2,
        button: egui::PointerButton,
    ) -> egui::RawInput {
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
    pub(crate) fn opened_uid_after_click(
        snapshot: &Snapshot,
        button: egui::PointerButton,
    ) -> Option<i64> {
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
    pub(crate) fn opened_uid_after_history_click(
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
            open: Some(Arc::new(open)),
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

    pub(crate) fn skill_window_state(pos: egui::Pos2) -> SkillWindowState {
        SkillWindowState {
            tabs: SkillTabs::default(),
            pos,
            size: SKILL_WINDOW_SIZE,
            source: SkillWindowSource::Live,
            gesture: WindowGesture::default(),
        }
    }

    pub(crate) fn skill_inner_rect_of(size: egui::Vec2) -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(egui::pos2(1.0, 1.0), size))
    }

    /// One row per fight for the same uid, told apart by `damage` — the
    /// whole point of issue #216's per-window source is that a uid present
    /// in both fights resolves to the right one.
    pub(crate) fn skill_source_row(uid: i64, damage: i64) -> PlayerRow {
        PlayerRow {
            uid,
            damage,
            ..sample_row(None)
        }
    }

    pub(crate) fn sample_skill_row(skill_id: i32) -> SkillRow {
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
            absorbed_total: 0,
            immune_total: 0,
        }
    }

    /// Two-frame click harness for `draw_skill_window`, the same shape as
    /// `opened_uid_after_click`: frame 1 lays the window out with no input
    /// and reads back where `value`'s text actually painted (not knowable
    /// ahead of a real run); frame 2 (the same `Context`, so the interact
    /// ids line up) sends a synthesized left click there and returns
    /// whatever this run reports the `X` glyph did, leaving `sort` mutated
    /// in place for the caller to inspect.
    pub(crate) fn click_skill_window_at(
        row: &PlayerRow,
        tabs: &mut SkillTabs,
        value: &str,
    ) -> bool {
        click_skill_window(row, tabs, |frame| frame.text_box(value).center())
    }

    /// The same two-frame harness, aimed by an arbitrary `locate` instead of
    /// by a painted string — the close button paints no text at all since
    /// issue #218 turned its `\u{2715}` into two line segments.
    pub(crate) fn click_skill_window(
        row: &PlayerRow,
        tabs: &mut SkillTabs,
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
                    tabs,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
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
                    tabs,
                    SkillWindowSource::Live,
                    &icons,
                    Opacity::OPAQUE,
                    &mut WindowGesture::default(),
                );
            },
        );
        output.drop_without_applying_deltas();
        clicked
    }

    // -- Encounter history view (issue #39) ---------------------------------

    /// Builds a throwaway `OverlayApp` for the history-view tests below —
    /// none of them exercise capture/settings/command plumbing, so every
    /// channel is a fresh, otherwise-unused pair.
    pub(crate) fn history_test_app() -> OverlayApp {
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

    pub(crate) fn history_test_record(title: &str) -> history::EncounterRecord {
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

    /// Issue #214, part 2: the pipeline thread's death used to be entirely
    /// silent — `main.rs` never checks its `JoinHandle`, so a panic in it
    /// left a frozen-but-normal-looking meter for the rest of the process's
    /// life. The dropped `Sender` it leaves behind is the signal, and the
    /// banner is what makes it visible.
    #[test]
    fn a_dead_pipeline_thread_raises_an_error_banner() {
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
        assert_eq!(app.status, StatusLine::Ok, "a live pipeline starts clean");

        drop(tx_snapshot);
        app.drain_snapshots();

        match &app.status {
            StatusLine::Error(message) => assert!(
                message.contains("pipeline"),
                "the banner must name what died, got {message:?}"
            ),
            other => panic!("expected an error banner, got {other:?}"),
        }
        assert_eq!(
            app.status_expires_at, None,
            "a dead pipeline never recovers, so its banner must not time out"
        );
    }

    /// The counterpart: a pipeline that is merely idle — connected, with
    /// nothing new to say — must leave the banner alone, or the overlay
    /// would cry wolf on every quiet frame.
    #[test]
    fn an_idle_pipeline_leaves_the_status_alone() {
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

        app.drain_snapshots();
        assert_eq!(app.status, StatusLine::Ok);

        tx_snapshot.send(demo_snapshot()).unwrap();
        app.drain_snapshots();
        assert_eq!(
            app.status,
            StatusLine::Ok,
            "a delivered snapshot is good news"
        );
    }

    /// The capture-init failure `main.rs` seeds through `with_status` is
    /// permanent and more specific than "the pipeline is gone"; a later
    /// pipeline disconnect at shutdown must not overwrite it with the
    /// vaguer message.
    #[test]
    fn a_dead_pipeline_does_not_overwrite_an_existing_error_banner() {
        let (tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded::<Snapshot>();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        )
        .with_status(StatusLine::Error("WinDivert is not installed".to_string()));

        drop(tx_snapshot);
        app.drain_snapshots();

        assert_eq!(
            app.status,
            StatusLine::Error("WinDivert is not installed".to_string()),
            "the more specific capture failure must survive"
        );
    }

    /// Counterpart to `a_dead_pipeline_does_not_overwrite_an_existing_error_banner`:
    /// a *transient* banner (Share/Export logs failure) is not "already up"
    /// in the sense that guard cares about — it is scheduled to clear itself
    /// in `expire_transient_status`, which runs after `drain_snapshots` in
    /// `ui()`'s per-frame order. Without distinguishing it from a permanent
    /// banner, a dead pipeline discovered mid-linger would stay masked by
    /// the stale clipboard message for up to `TRANSIENT_STATUS_LINGER`
    /// instead of raising the fatal banner immediately.
    #[test]
    fn a_dead_pipeline_overwrites_a_transient_error_banner() {
        let (tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded::<Snapshot>();
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );

        app.raise_transient_status("Copy screenshot failed: oops".to_string(), Instant::now());
        assert!(
            app.status_expires_at.is_some(),
            "sanity: banner is transient"
        );

        drop(tx_snapshot);
        app.drain_snapshots();

        match &app.status {
            StatusLine::Error(message) => assert!(
                message.contains("pipeline"),
                "the permanent banner must replace the transient one, got {message:?}"
            ),
            other => panic!("expected the permanent pipeline-dead banner, got {other:?}"),
        }
        assert_eq!(
            app.status_expires_at, None,
            "the permanent banner must not inherit the transient banner's expiry"
        );
    }
}
