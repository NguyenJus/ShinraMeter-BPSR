//! ShinraMeter-style egui overlay (plan §T4.1).
//!
//! `OverlayApp` is pure "snapshot in, commands out": it renders a
//! `bpsr_meter::Snapshot` handed to it over a channel and emits `UiCommand`s
//! for the app layer to act on. No threads or channels are created in this
//! module beyond the `crossbeam_channel` endpoints eframe's caller hands in.

use std::time::Duration;

use bpsr_meter::{Class, EncounterInfo, PlayerRow, Role, Snapshot};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::fonts;
use crate::icons::{ClassIcons, GlyphIcon, GlyphIcons, ImagineIcons, ToolbarIcon, ToolbarIcons};
use crate::imagines;
use crate::settings::{ColumnKind, Settings};

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
    /// Issue #131: clears the learned scene -> final-boss map, both
    /// in-process and its on-disk cache. Sent from the header dropdown's
    /// "Forget learned bosses" item (`draw_header_menu`); handled by
    /// `pipeline::run` on the pipeline thread, not here — same as `Reset`.
    ForgetLearnedBosses,
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
    /// `demo_enabled()` cached at construction so `ui()` doesn't re-read the
    /// env var every frame; also lets `ui()` keep demo mode's synthetic
    /// snapshot from being clobbered by the per-frame `rx_snapshot` drain
    /// below (see that call site).
    demo_mode: bool,
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

/// `(name, class, damage, crit_pct, lucky_pct, hits, deaths, imagines)` for
/// one `DEMO_ROWS` entry. Named so the array below reads as one type, not
/// clippy's `type_complexity` bait.
type DemoRow = (
    &'static str,
    Class,
    i64,
    f32,
    f32,
    u64,
    u32,
    [Option<i32>; 2],
);

/// `(name, class, damage, crit_pct, lucky_pct, hits, deaths, imagines)` for
/// each demo row, in descending-damage order (issue #148). A realistic
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
/// nearly all-in on one stat, `Zog` splits close to evenly (and so sits
/// lower on both axes than the all-in rows), and `Glorbaxian`/`Thudd` land
/// in between. Hits and deaths are shaped on a real tank-vs-DPS ratio —
/// `Thudd` racks up far more hits (a tank's rotation is faster/lower per
/// hit) and nobody but `Glorbaxian` dies.
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
    ),
];

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
            |(i, &(name, class, damage, crit_pct, lucky_pct, hits, deaths, imagine_ids))| {
                PlayerRow {
                    uid: i as i64 + 1,
                    name: name.to_string(),
                    class: Some(class),
                    ability_score: None,
                    season_strength: None,
                    imagines: imagine_ids,
                    damage,
                    dps: damage as f64 / (duration_ms as f64 / 1000.0),
                    share_pct: damage as f32 / row_damage_sum as f32 * 100.0,
                    crit_pct,
                    lucky_pct,
                    hits,
                    deaths,
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
    ) -> Self {
        // Demo seed (see `demo_enabled`/`demo_snapshot` above). Cached once
        // here rather than re-called, so `ui()` below can reuse the same
        // answer every frame instead of re-reading the env var.
        let demo_mode = demo_enabled();
        let snapshot = initial_snapshot(demo_mode);
        Self {
            snapshot,
            status: StatusLine::Ok,
            settings,
            rx_snapshot,
            tx_command,
            tx_settings,
            icons: None,
            window_gesture: WindowGesture::default(),
            last_dpi_probe: None,
            pending_screenshot_bound: None,
            demo_mode,
        }
    }

    pub fn with_status(mut self, status: StatusLine) -> Self {
        self.status = status;
        self
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
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_snapshots();

        let ctx = ui.ctx().clone();
        apply_theme(&ctx);

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
        handle_share_screenshot(&ctx, &mut self.pending_screenshot_bound, |image| {
            crate::platform::write_clipboard_image(&image);
        });

        // Loaded once, lazily: the `egui::Context` above isn't available yet
        // at `OverlayApp::new`, so the first frame is what actually uploads
        // the icon textures (issues #9, #41); every later frame reuses them.
        let icons = self.icons.get_or_insert_with(|| Icons::load(&ctx));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(PANEL_FILL)
                    .stroke(egui::Stroke::new(PANEL_BORDER_WIDTH, PANEL_BORDER_COLOR))
                    .corner_radius(egui::CornerRadius::same(PANEL_CORNER_RADIUS)),
            )
            .show(ui, |ui| {
                // First, so the header buttons drawn afterwards stay on top of
                // the corner zones they overlap.
                draw_resize_handles(ui, &ctx, &mut self.window_gesture);
                // Issue #96 (PR #98 review): whether the Share button fired
                // a screenshot request this frame — if so, the row bound
                // this same frame computes below is stashed into
                // `pending_screenshot_bound` for whenever the async reply
                // lands, instead of leaving that field for the crop to read
                // fresh (and possibly stale) at reply time.
                let screenshot_requested = draw_header(
                    ui,
                    &ctx,
                    &self.snapshot,
                    &self.tx_command,
                    SettingsHandle {
                        settings: &mut self.settings,
                        tx_settings: &self.tx_settings,
                    },
                    icons,
                    &mut self.window_gesture,
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
                // false])`.
                let rows_top = ui.cursor().top();
                let rows_area_height = ui.available_height();
                draw_rows(ui, &self.snapshot, &self.settings.ordered_columns(), icons);
                if screenshot_requested {
                    self.pending_screenshot_bound = Some(rows_content_bottom_y(
                        rows_top,
                        self.snapshot.rows.len(),
                        ROW_HEIGHT,
                        rows_area_height,
                    ));
                }
            });

        // Read once and share with both trackers rather than each calling
        // `ctx.input` separately — also what lets `minimized` be threaded
        // through both as the exact same value for the same frame.
        let (outer_rect, inner_rect, minimized) = ctx.input(|i| {
            let viewport = i.viewport();
            (viewport.outer_rect, viewport.inner_rect, viewport.minimized)
        });
        track_window_position(outer_rect, minimized, &mut self.settings, &self.tx_settings);
        track_window_size(inner_rect, minimized, &mut self.settings, &self.tx_settings);

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
fn draw_header(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    snapshot: &Snapshot,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
    gesture: &mut WindowGesture,
) -> bool {
    let title = encounter_title(&snapshot.encounter);
    let subtitle = encounter_subtitle(&snapshot.encounter);
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
    // are the header's background, so they run behind the stat-pill row too
    // and stop exactly at the band's bottom edge, where the first player row
    // starts. Derived from `band_height`, never a literal.
    let wash_height = band_height - HEADER_WASH_INSET;
    draw_header_wash(ui, panel, icons, wash_height);

    let drag_surface = ui.interact(band, ui.id().with("title_bar"), egui::Sense::drag());
    if drag_surface.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::Grab);
    }
    // Once per gesture: this only captures the anchor the drag is measured
    // against. The actual per-frame repositioning is `drive_window_gesture`.
    if drag_surface.drag_started_by(egui::PointerButton::Primary) {
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
    let chevron_response = menu_chevron(ui, chevron_rect(title_row));
    // `CloseOnClickOutside` rather than the default `CloseOnClick` (issue
    // #93): with the Columns checkboxes now direct children of this popup
    // (no submenu layer to defer the close decision to, see
    // `draw_header_menu`'s doc comment), the default would dismiss the
    // whole dropdown on every checkbox toggle. Minimize/Close call
    // `ui.close()` themselves to still dismiss on click.
    egui::Popup::menu(&chevron_response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            draw_header_menu(ui, ctx, tx_command, settings, icons);
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
        toggle_cluster(ui, tx_command, icons)
    })
    .inner
}

// -- status indicators (issue #62, #82) -----------------------------------
//
// The source's fourth stat-row cell: a click-through LED, a cloud-upload LED
// and a queue gauge, in a 22pt pill. We had none of those features, so all
// three used to render **in their off state and inert** — no click
// handling, no settings, no tooltip. Issue #62 was explicit that a use for
// these slots would be decided later; issue #82 decides two of them: the
// click-through and cloud-upload LEDs are repurposed as real buttons
// (Share — copy a screenshot to the clipboard — and Reset, moved out of the
// header dropdown), leaving only the queue gauge ring inert, since there is
// still no upload queue for it to show.

/// Tint the still-inert queue ring (and its check glyph) is painted with —
/// the source's `OffBrush="#1fff"`.
const TOGGLE_OFF_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x11);
/// Tint the toggle cluster's two real buttons are painted with — the same
/// half-white `TOOLBAR_ICON_TINT` every other clickable icon in this module
/// uses, now that these two read as active controls rather than inert
/// decoration.
const TOGGLE_ACTIVE_COLOR: egui::Color32 = TOOLBAR_ICON_TINT;
/// Circular hover wash painted behind a toggle-cluster button, matching the
/// oval pill's own shape rather than a foreign square badge.
const TOGGLE_HOVER_FILL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 30);
const TOGGLE_MOUSE_SIDE: f32 = 12.0;
const TOGGLE_CLOUD_SIDE: f32 = 14.0;
const TOGGLE_QUEUE_SIDE: f32 = 14.0;
const TOGGLE_QUEUE_GLYPH_SIDE: f32 = 6.0;
const TOGGLE_GAP: f32 = 5.0;
const TOGGLE_PAD_X: f32 = 4.0;

/// One toggle-cluster button's hit box, hover highlight and accessible
/// label. The painted glyph itself is the caller's job — Share and Reset
/// draw from two different icon sets (`GlyphIcon` and `ToolbarIcon`) — so
/// this only owns the interaction chrome shared by both. Same
/// hand-supplied `widget_info`/`on_hover_text` shape as `menu_chevron`, and
/// for the same reason: a raw `interact` `Response` carries no `WidgetInfo`
/// from anywhere.
fn toggle_button(ui: &egui::Ui, rect: egui::Rect, label: &str) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("toggle_cluster", label)),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter()
            .circle_filled(rect.center(), rect.width() / 2.0 + 2.0, TOGGLE_HOVER_FILL);
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), label)
    });
    response.on_hover_text(label)
}

/// Paints the toggle cluster: the Share and Reset buttons (issue #82) and
/// the still-inert queue gauge ring (an empty ring — the source's `Ellipse
/// 14x14 Fill="#0aaa"` has alpha 0, so only the stroke is drawn — with the
/// check glyph centered in it; no backlog arc, since there is still no
/// queue for it to show). All in one `PILL_FILL` oval, matching the
/// DPS/damage pills' chrome.
///
/// Returns whether Share was clicked this frame (issue #96, PR #98 review)
/// — `draw_header` propagates this up so `OverlayApp::ui` knows to stash
/// this frame's row-bottom bound for the async screenshot reply.
fn toggle_cluster(ui: &mut egui::Ui, tx_command: &Sender<UiCommand>, icons: &Icons) -> bool {
    let height = ui.spacing().interact_size.y;
    let width = 2.0 * TOGGLE_PAD_X
        + TOGGLE_MOUSE_SIDE
        + TOGGLE_GAP
        + TOGGLE_CLOUD_SIDE
        + TOGGLE_GAP
        + TOGGLE_QUEUE_SIDE;
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
    if toggle_button(ui, share_rect, "Copy screenshot to clipboard").clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        screenshot_requested = true;
    }
    if let Some(share) = icons.glyphs.get(GlyphIcon::Share) {
        ui.painter()
            .image(share.id(), share_rect, UV_FULL, TOGGLE_ACTIVE_COLOR);
    }
    x += TOGGLE_MOUSE_SIDE + TOGGLE_GAP;

    // Reset (issue #82): moved out of the header dropdown — `draw_header_menu`
    // used to be its only trigger — into a one-click slot here, reusing the
    // same `ToolbarIcon::Reset` texture and the same `UiCommand::Reset`.
    let reset_rect = egui::Rect::from_center_size(
        egui::pos2(x + TOGGLE_CLOUD_SIDE / 2.0, y),
        egui::Vec2::splat(TOGGLE_CLOUD_SIDE),
    );
    if toggle_button(ui, reset_rect, "Reset").clicked() {
        let _ = tx_command.try_send(UiCommand::Reset);
    }
    if let Some(reset) = icons.toolbar.get(ToolbarIcon::Reset) {
        ui.painter()
            .image(reset.id(), reset_rect, UV_FULL, TOGGLE_ACTIVE_COLOR);
    }
    x += TOGGLE_CLOUD_SIDE + TOGGLE_GAP;

    // The queue gauge stays inert — see the section comment above.
    let queue_center = egui::pos2(x + TOGGLE_QUEUE_SIDE / 2.0, y);
    ui.painter().circle_stroke(
        queue_center,
        TOGGLE_QUEUE_SIDE / 2.0,
        egui::Stroke::new(1.0, TOGGLE_OFF_COLOR),
    );
    if let Some(check) = icons.glyphs.get(GlyphIcon::Check) {
        let icon_rect =
            egui::Rect::from_center_size(queue_center, egui::Vec2::splat(TOGGLE_QUEUE_GLYPH_SIDE));
        ui.painter()
            .image(check.id(), icon_rect, UV_FULL, TOGGLE_OFF_COLOR);
    }

    screenshot_requested
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
///    `EncounterInfo::is_boss`). This now outranks `scene_boss_name` —
///    inverted from issue #125's original precedence — because a raid can
///    string several *different* final bosses together in one instance
///    (repo owner, issue #131): `Meter::scene_bosses` only ever remembers
///    one boss per scene and overwrites, so once a second or third raid
///    boss is actually engaged, showing the *previous* one instead of the
///    boss currently being fought would be actively wrong, not just stale.
///    A single-final-boss dungeon is unaffected: nothing else is ever
///    `is_boss` there once the run's target is the remembered boss itself.
/// 2. else `scene_boss_name` — the current dungeon's *remembered final
///    boss* (`Meter::scene_bosses`), if one has been learned. This is what
///    covers both "just walked in, nothing engaged yet" (`boss_monster_id`
///    is still `None`) and the issue #125 case: `recompute_boss` selected a
///    non-boss mid-dungeon mech or add as `boss_uid` (`is_boss` false), so
///    branch 1 doesn't apply, but the header still shouldn't go blank or
///    fall through to "No target" when the dungeon's real boss is already
///    known.
/// 3. else the pre-#125 fallback: blank for a non-boss pull with nothing
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
/// Known limitation (issue #131, not fixed here): on *entry* to a
/// multi-boss raid, before anything has been engaged this session,
/// `scene_boss_name` is whichever boss `Meter::scene_bosses` last latched
/// for that scene — not necessarily the *first* one the raid will actually
/// fight. The header only becomes correct once a boss is engaged and branch
/// 1 takes over.
fn encounter_title(e: &EncounterInfo) -> String {
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
        None => "No target".to_string(),
        Some(_) => String::new(),
    }
}

/// Header subtitle text (issue #9 slice 2): the scene name when known, else
/// its raw scene id, else `None` — `draw_header` paints the subtitle row
/// blank in that case. The row's space is reserved either way (issue #91,
/// `header_text_band_height`); only its ink is conditional.
fn encounter_subtitle(e: &EncounterInfo) -> Option<String> {
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
    let left = row.left() + HEADER_GUTTER_WIDTH + HEADER_TEXT_PAD_X;
    let right = (row.right() - HEADER_RIGHT_CONTROL_WIDTH).max(left);
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
    let rect = header_text_rect(row);
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
/// oversized emblem share one rect, so both now run the full band and both
/// stop dead at its bottom edge, flush with the first player row. No fixed
/// constant is left to drift out of sync with the content it sits behind.
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
/// (not the drag band); `height` (issue #91, `header_band_height` less
/// `HEADER_WASH_INSET`) is what actually bounds the wash — the whole header
/// band, stat-pill row included, stopping exactly where the first player row
/// begins (`wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row`).
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
/// Forget learned bosses (issue #131), a separator, Minimize to tray, a
/// separator, then Close. Reset used to be the first item here but moved
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
fn draw_header_menu(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tx_command: &Sender<UiCommand>,
    settings: SettingsHandle<'_>,
    icons: &Icons,
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

    // Issue #131: the escape hatch for a stale learned boss name (e.g. after
    // a game patch changes a dungeon's final boss) — see
    // `scene_bosses_cache`'s doc comment for why nothing invalidates the
    // cache automatically. Placed here (the header dropdown) rather than
    // the tray's native Win32 context menu (`platform::install_tray`)
    // because this menu is already the smaller, cross-platform, plain-egui
    // surface with a `tx_command` in scope — the tray menu would need new
    // HMENU/message-id plumbing in `platform.rs` for what is otherwise a
    // one-line addition here.
    if ui.button("Forget learned bosses").clicked() {
        let _ = tx_command.try_send(UiCommand::ForgetLearnedBosses);
        ui.close();
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

/// Issue #74: whether a gesture of `kind` ending should force a DWM frame
/// recompute (`platform::force_frame_recompute`). Only resizes actually
/// change the window's size, and the opaque-gray symptom this works around
/// is a DWM composition artifact left behind by a resize — a pure `Move`
/// gesture ending has nothing to recompute a frame for.
///
/// Pulled out of `drive_window_gesture` as a pure function, the same way
/// `row_bar_frac`/`share_bar_paints`/`column_anchors` extract pure decisions
/// out of `Ui`-dependent code elsewhere in this file, so this call-site
/// choice is unit-testable without a window.
fn gesture_end_needs_frame_recompute(kind: GestureKind) -> bool {
    matches!(kind, GestureKind::Resize(_))
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
        if gesture_end_needs_frame_recompute(kind) {
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
fn draw_resize_handles(ui: &mut egui::Ui, ctx: &egui::Context, gesture: &mut WindowGesture) {
    let window = ctx.input(|i| i.viewport_rect());
    // `ResizeDirection` is not `Hash`, so the zone's position in the array is
    // what keeps the eight ids distinct.
    for (index, (zone, direction, cursor)) in resize_zones(window).into_iter().enumerate() {
        let handle = ui.interact(zone, ui.id().with(("resize", index)), egui::Sense::drag());
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
    columns: &[ColumnKind],
    icons: &Icons,
) -> egui::Vec2 {
    // The enabled-column set (and therefore the column widths) is identical
    // for every row in a frame, so it's computed once here rather than once
    // per row inside `draw_row`.
    let stat_columns = stat_columns_for(columns);
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
                kinds: columns,
                columns: &stat_columns,
                anchors: &anchors,
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
                draw_row(ui, row, &layout, icons, top_damage, content_width);
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

/// The horizontal clip rect for one stat column's painted text: bounded on
/// the right by that column's anchor (where its right-aligned text ends)
/// and on the left by exactly one nominal `width` — the budget in-range
/// text for the column is designed, and tested
/// (`widest_formatted_text_fits_its_column_width_budget`), to fit.
///
/// This is what keeps an out-of-range value (e.g. a packet-decoded
/// `ability_score`/`season_strength` past the in-game ceiling `StatColumn`'s
/// `width` budget assumes — see `ColumnKind::spec`) from painting
/// arbitrarily far left across the row: `draw_row` clips every column's text
/// draw to this rect rather than trusting the formatted string to fit
/// `width`, so an overlong string loses its leading glyphs after one
/// column's worth instead of running over its neighbors.
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
) {
    let desired_size = egui::vec2(row_width, ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

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
    // runtime clip is needed) with a name tooltip on hover; an empty slot,
    // an id outside the curated table, or a texture that failed to decode
    // all degrade to the same blank-circle placeholder — one branch, never
    // a panic (D4's runtime-degrade path).
    for (i, slot) in imagine_slots.into_iter().enumerate() {
        let filled = row.imagines[i]
            .and_then(imagines::imagine_of_skill_id)
            .and_then(|im| icons.imagines.get(im.icon).map(|texture| (im, texture)));
        match filled {
            Some((im, texture)) => {
                ui.painter()
                    .image(texture.id(), slot, UV_FULL, CLASS_ICON_TINT);
                ui.interact(
                    slot,
                    ui.id().with(("imagine", row.uid, i)),
                    egui::Sense::hover(),
                )
                .on_hover_text(im.name);
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
    paint_text(
        ui.painter(),
        rect.left_center() + egui::vec2(name_offset, 0.0),
        egui::Align2::LEFT_CENTER,
        &name,
        regular(FONT_SIZE_ROW),
        egui::Color32::WHITE,
        false,
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

/// Square side of the per-row class icon (issue #9). Matches the source's
/// `Path 18x18`.
const ICON_SIZE: f32 = 18.0;

/// Gap on both sides of the icon: between the row's left edge and the icon,
/// and between the icon and the Imagine gutter that follows it. `3.5` so
/// the class-icon portion of `ICON_GUTTER_WIDTH` lands exactly on the
/// source's 18px glyph centered in a fixed 25px `SharedSizeGroup="p0"`
/// column — `25.0` is what `ICON_GUTTER_WIDTH` reverts to once the
/// `IMAGINE_GUTTER_WIDTH` addend below is deleted (D4's takedown).
const ICON_MARGIN: f32 = 3.5;

/// Class icon tint (source `Fill="#ddd"`).
const CLASS_ICON_TINT: egui::Color32 = egui::Color32::from_rgb(0xDD, 0xDD, 0xDD);

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Square side of each Imagine slot (issue #33) — subordinate to the
/// 18x18 class icon.
const IMAGINE_SIZE: f32 = 14.0;

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
fn default_inner_height() -> f32 {
    let rows = DEFAULT_VISIBLE_ROWS as f32 * ROW_HEIGHT;
    header_band_height(BUTTON_ROW_HEIGHT) + SEPARATOR_HEIGHT + rows + ITEM_SPACING_Y
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
    let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
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

/// Panel fill: translucent near-black, the overlay's own value rather than the
/// source's `WindowData.DefaultBackgroundColor` `#232830` @ 0.5. That slate
/// grey reads as washed-out over game footage; the original ShinraMeter
/// silhouette is near-black, so we keep `#121216` at 200/255 here. Fixed
/// constants deliberately — the source binds all three of these to a settings
/// VM, and user-configurable chrome is out of scope for now.
const PANEL_FILL: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(18, 18, 22, 200);
/// Panel border: `DefaultBorderColor` `#717b85`, at the same 0.5 opacity the
/// source applies to the whole Border (fill and stroke alike).
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
        for &(name, _, _, _, _, _, _, ids) in &DEMO_ROWS {
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
        let mut app = OverlayApp::new(rx_snapshot, tx_command, tx_settings, Settings::default());
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
        let mut app = OverlayApp::new(rx_snapshot, tx_command, tx_settings, Settings::default());
        app.demo_mode = false;

        tx_snapshot.send(rows_test_snapshot(2)).unwrap();
        app.drain_snapshots();

        assert_eq!(app.snapshot.rows.len(), 2);
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
        // The toggle cluster (decision 5, issue #82): a fixed-width pill,
        // not measured text.
        let toggles = 2.0 * TOGGLE_PAD_X
            + TOGGLE_MOUSE_SIDE
            + TOGGLE_GAP
            + TOGGLE_CLOUD_SIDE
            + TOGGLE_GAP
            + TOGGLE_QUEUE_SIDE;

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

    /// Share and Reset (issue #82) are real buttons now, painted at
    /// `TOGGLE_ACTIVE_COLOR`; only the still-inert queue gauge's check
    /// glyph keeps the source's `OffBrush="#1fff"` tint.
    #[test]
    fn the_toggle_cluster_renders_two_active_buttons_and_one_inert_gauge() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let share = icons.glyphs.get(GlyphIcon::Share).unwrap().id();
        let reset = icons.toolbar.get(ToolbarIcon::Reset).unwrap().id();
        let check = icons.glyphs.get(GlyphIcon::Check).unwrap().id();

        let mut blits = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons);
        });
        for clipped in &output.shapes {
            collect_image_texture_tints(&clipped.shape, &mut blits);
        }
        output.drop_without_applying_deltas();

        for expected in [share, reset] {
            let tint = blits
                .iter()
                .find(|(id, _)| *id == expected)
                .map(|(_, c)| *c);
            assert_eq!(
                tint,
                Some(TOGGLE_ACTIVE_COLOR),
                "{expected:?} was not blitted at TOGGLE_ACTIVE_COLOR: {blits:?}"
            );
        }
        let check_tint = blits.iter().find(|(id, _)| *id == check).map(|(_, c)| *c);
        assert_eq!(
            check_tint,
            Some(TOGGLE_OFF_COLOR),
            "the queue gauge's check glyph was not blitted at TOGGLE_OFF_COLOR: {blits:?}"
        );
    }

    /// The queue gauge stays strictly non-interactive (issue #62, #82): no
    /// click handling, no hover cursor, no tooltip that implies it works.
    /// Share and Reset, by contrast, must each expose exactly one `Button`
    /// accesskit node — so the tree has exactly two, never three.
    #[test]
    fn the_queue_gauge_stays_inert_while_share_and_reset_are_real_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            toggle_cluster(ui, &tx_command, &icons);
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
            button_count, 2,
            "expected exactly Share and Reset to expose a Button role, got {button_count}"
        );
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
            toggle_cluster(ui, &tx_command, &icons);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let share_pos = accessible_rect_for_label(&update, "Copy screenshot to clipboard").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(share_pos), |ui| {
            toggle_cluster(ui, &tx_command, &icons);
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
            toggle_cluster(ui, &tx_command, &icons);
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let reset_pos = accessible_rect_for_label(&update, "Reset").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(reset_pos), |ui| {
            toggle_cluster(ui, &tx_command, &icons);
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
        let mut app = OverlayApp::new(rx_snapshot, tx_command, tx_settings, Settings::default());

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
    /// text rows — `draw_header` sizes it to
    /// `header_band_height - HEADER_WASH_INSET`, one rect carrying both the
    /// gradient and the oversized emblem. What it must still never do is
    /// bleed past the band into the first player row, which is exactly where
    /// the old fixed `98.0`pt wash went wrong.
    #[test]
    fn wash_covers_the_stat_pill_row_but_stops_at_the_first_player_row() {
        let panel = wash_test_panel();
        let button_row_height = 18.0;
        let text_band = header_text_band_height();
        let band = header_band_height(button_row_height);
        let wash = header_wash_rect(panel, band - HEADER_WASH_INSET);
        let stat_pill_row_top = panel.top() + text_band + HEADER_STAT_ROW_GAP;
        let stat_pill_row_bottom = stat_pill_row_top + button_row_height;
        let first_player_row_top = panel.top() + band;

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
            "wash bottom {} bleeds past the header band into the first player \
             row at {first_player_row_top}",
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
    #[test]
    fn the_wash_gradient_spans_the_whole_header_band() {
        let snapshot = header_test_snapshot(30_100_000_000);
        let frame = header_painted_boxes(&snapshot);
        let gradient = frame.gradient_box();

        let band = header_band_height(BUTTON_ROW_HEIGHT);
        assert!(
            (gradient.height() - (band - HEADER_WASH_INSET)).abs() < 0.01,
            "the wash gradient is {}pt tall, not the header band's {}pt",
            gradient.height(),
            band - HEADER_WASH_INSET
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
    fn chrome_border_and_fill_are_translucent() {
        assert_eq!(PANEL_BORDER_COLOR.a(), 128);
        let fill_alpha = PANEL_FILL.a();
        assert!(fill_alpha > 0 && fill_alpha < 255, "{fill_alpha}");
    }

    /// The panel is deliberately *not* the source's slate `#232830` — that
    /// reads as washed-out grey over game footage. Lock the near-black.
    #[test]
    fn panel_fill_is_near_black_not_slate() {
        // Compare through the same constructor: `Color32` stores premultiplied
        // channels, so `to_tuple()` would not round-trip the (18, 18, 22).
        assert_eq!(
            PANEL_FILL,
            egui::Color32::from_rgba_unmultiplied(18, 18, 22, 200)
        );
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
        let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
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
        // 56.0 and landing here at `427.0`.
        assert_eq!(default_inner_width(), 427.0);
    }

    #[test]
    fn default_inner_width_exceeds_the_default_stat_columns_width() {
        let columns_width: f32 = stat_columns_for(&Settings::default().ordered_columns())
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
        texts: Vec<(String, egui::Rect)>,
        meshes: Vec<egui::Rect>,
    }

    impl RowFrame {
        /// The union of every text shape painted for `value` (a player
        /// name here).
        fn text_box(&self, value: &str) -> egui::Rect {
            self.texts
                .iter()
                .filter(|(painted, _)| painted == value)
                .map(|(_, rect)| *rect)
                .reduce(egui::Rect::union)
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
            draw_rows(ui, snapshot, &Settings::default().ordered_columns(), &icons);
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
            content_size = draw_rows(ui, snapshot, &Settings::default().ordered_columns(), &icons);
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
        assert!(gesture_end_needs_frame_recompute(resize));
    }

    #[test]
    fn a_finished_move_does_not_need_a_frame_recompute() {
        // A pure move never changes the window's size, so there is nothing
        // for a DWM frame recompute to fix.
        assert!(!gesture_end_needs_frame_recompute(GestureKind::Move));
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
}
