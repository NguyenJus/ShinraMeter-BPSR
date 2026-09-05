//! History surface: the encounter list, its bar, and its rows.

use super::*;

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
pub(super) enum OverlayView {
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
pub(super) struct HistoryUi {
    /// Newest-first summaries from the last `Listed` reply.
    pub(super) encounters: Vec<history::EncounterSummary>,
    /// The encounter currently being read, already rebuilt into a `Snapshot`
    /// so `draw_rows` can render it unchanged — a past fight looks pixel
    /// identical to a live one.
    ///
    /// Issue #350: `Arc`-wrapped, and only ever reassigned here (a fresh
    /// `Loaded`/`Missing` reply or a "← Back"/"← Live" click) — never once
    /// per frame. `OverlayApp::ui`'s `history_open` local used to deep-clone
    /// the whole `OpenEncounter` (its `Snapshot`, rows and all) on every
    /// single frame just to get a value it could hand `draw_header`/
    /// `draw_rows`; cloning the `Arc` instead is a refcount bump regardless
    /// of how large the held `Snapshot` is, and consecutive frames between
    /// replies share the exact same allocation (`Arc::ptr_eq`).
    pub(super) open: Option<Arc<OpenEncounter>>,
    /// A `HistoryEvent::Failed` message worth showing, cleared on the next
    /// successful reply.
    pub(super) error: Option<String>,
    /// True between firing a request and its reply landing — the view shows
    /// "Loading…" rather than a stale empty list.
    pub(super) pending: bool,
    /// Latches "Clear all" into a confirming state after one click; a second
    /// click while this is true actually fires `HistoryHandle::clear`. Reset
    /// on every other history-bar/list interaction, so leaving and
    /// returning to the list never leaves it primed.
    pub(super) confirm_clear: bool,
    /// The id of the newest in-flight `Load` request, if any. Rows stay
    /// clickable while one is in flight, so a `Loaded`/`Missing` reply
    /// carrying any other id belongs to a click the user has already
    /// superseded, and is dropped.
    pub(super) pending_load_id: Option<i64>,
}

/// One saved encounter, rebuilt for display: the id (needed for the delete
/// button while it's open) plus everything `draw_header`/`draw_rows` need.
#[derive(Clone)]
pub(super) struct OpenEncounter {
    // `id` is what a breakdown window opened from this fight stores as its
    // `SkillWindowSource::History` (issue #216), so a later frame can tell
    // "still the same fight" from "a different one is open now".
    // `ended_at_ms` rounds the DTO out to match `EncounterSummary`'s shape
    // (and is what a future "delete the fight I'm looking at" button would
    // need), but WP3's bar only offers delete from the list — so it is not
    // read yet.
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) subtitle: Option<String>,
    #[allow(dead_code)]
    pub(super) ended_at_ms: u64,
    pub(super) snapshot: Snapshot,
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
pub(super) fn share_active_for_view(view: &OverlayView) -> bool {
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
pub(super) fn screenshot_row_count(view: &OverlayView, live_row_count: usize) -> usize {
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
pub(super) fn resolve_screenshot_row_count(
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
pub(super) struct HistoryHeader<'a> {
    pub(super) title: &'a str,
    pub(super) subtitle: Option<&'a str>,
}

/// The ceiling on how many encounters the list requests, used when
/// `Settings::history_max_encounters` is `0` — "prune nothing by count",
/// which has no matching `LIMIT`. Equal to the settings cap, so the list can
/// never hide a row retention has kept.
pub(super) const HISTORY_LIST_CEILING: u32 = Settings::HISTORY_MAX_ENCOUNTERS_CAP;

/// Width of the trailing delete button painted into each history row.
pub(super) const HISTORY_DELETE_WIDTH: f32 = 18.0;

/// Left/right text inset inside a history row, matching the row list's own
/// breathing room rather than introducing a new metric scale.
pub(super) const HISTORY_ROW_PADDING: f32 = 8.0;

/// The header's title/subtitle selection: a historical fight's saved name
/// (spec DECISION D7) when `history` is `Some`, the live encounter's derived
/// name otherwise. Pulled out of `draw_header` as the one pure extraction
/// WP3 permits, so it is testable without an `egui::Ui`.
pub(super) fn header_text(
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
    pub(super) fn poll_history(&mut self) {
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
                    state.open = Some(Arc::new(OpenEncounter {
                        id,
                        title: record.title.clone(),
                        subtitle: record.subtitle.clone(),
                        ended_at_ms: record.ended_at_ms,
                        snapshot: record.to_snapshot(),
                    }));
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
    pub(super) fn history_list_limit(&self) -> u32 {
        match self.settings.history_max_encounters {
            0 => HISTORY_LIST_CEILING,
            limit => limit.min(HISTORY_LIST_CEILING),
        }
    }

    /// Switches to the history view and asks for the list (issue #39).
    pub(super) fn open_history(&mut self) {
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
pub(super) fn draw_history(
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
pub(super) fn draw_history_bar(ui: &mut egui::Ui, state: &HistoryUi) -> HistoryBarAction {
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
pub(super) fn draw_history_list(ui: &mut egui::Ui, state: &HistoryUi) -> Option<HistoryRowAction> {
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
pub(super) fn draw_history_row(
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
pub(super) enum HistoryRowAction {
    Open(i64),
    Delete(i64),
}

/// What a click on the bar produced.
pub(super) enum HistoryBarAction {
    None,
    Live,
    Back,
    ClearAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
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
            open: Some(Arc::new(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(3),
            })),
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
            open: Some(Arc::new(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(9),
            })),
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
            open: Some(Arc::new(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(9),
            })),
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

    /// Issue #350: `OverlayApp::ui`'s per-frame `history_open` local used to
    /// deep-clone the whole `OpenEncounter` (`Snapshot`, rows and all) every
    /// single frame; `HistoryUi::open` being `Arc`-wrapped means that local
    /// is now a refcount bump. This is the property that actually matters:
    /// two frames' worth of `state.open.clone()` — the exact call
    /// `OverlayApp::ui` makes — with no `Loaded`/`Missing` reply landing in
    /// between must hand back the *same* allocation, not two copies of an
    /// equal one.
    #[test]
    fn consecutive_frames_without_a_new_reply_share_the_same_open_encounter_arc() {
        let mut app = history_test_app();
        app.open_history();
        let OverlayView::History(state) = &mut app.view else {
            panic!("expected the History view");
        };
        state.pending_load_id = Some(7);

        app.tx_history
            .send(history::writer::HistoryEvent::Loaded {
                id: 7,
                record: Box::new(history_test_record("Steady Fight")),
            })
            .unwrap();
        app.poll_history();

        let OverlayView::History(state) = &app.view else {
            panic!("expected the History view");
        };
        // No new reply lands between these two "frames" — `state.open`
        // itself is never reassigned, so every clone of it must point at
        // the one `OpenEncounter` the `Loaded` reply above allocated.
        let frame_one = state.open.clone().expect("the reply above opened it");
        let frame_two = state.open.clone().expect("still open, unchanged");
        assert!(
            Arc::ptr_eq(&frame_one, &frame_two),
            "consecutive frames must share the same Arc<OpenEncounter> allocation"
        );
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
        state.open = Some(Arc::new(OpenEncounter {
            id: 9,
            title: "Stale Fight".to_string(),
            subtitle: None,
            ended_at_ms: 0,
            snapshot: header_test_snapshot(1_000),
        }));

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
            open: Some(Arc::new(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(3),
            })),
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

    /// Issue #298: strengthens the test above's loose `>` check into a
    /// concrete pixel accounting — proving not just that `rows_top` moves
    /// past the "← Live"/"← Back" bar, but that a Share screenshot sized to
    /// the *correct* bound fully contains the open encounter's rows, while
    /// the same image sized to the naive, pre-#219 bound (measured before
    /// `draw_history`'s own bar and separator, the way the outer panel's
    /// stale `rows_top` used to be) would truncate real row pixels off the
    /// bottom — exactly the symptom issue #298 reports.
    #[test]
    fn share_crop_bound_for_an_open_encounter_fully_contains_its_rows_past_the_history_bar() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let settings = Settings::default();
        let row_count = 3;
        let mut state = HistoryUi {
            open: Some(Arc::new(OpenEncounter {
                id: 1,
                title: "Fight".to_string(),
                subtitle: None,
                ended_at_ms: 0,
                snapshot: rows_test_snapshot(row_count),
            })),
            ..HistoryUi::default()
        };
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut back_to_live = false;

        let mut pre_call_top = 0.0;
        let mut reported_rows_top = 0.0;
        let mut rows_area_height = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            pre_call_top = ui.cursor().top();
            let (rows_top, area_height) = draw_history(
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
            rows_area_height = area_height;
        })
        .drop_without_applying_deltas();

        let bar_height = reported_rows_top - pre_call_top;
        assert!(
            bar_height > 0.0,
            "the history bar and separator must occupy real space (got {bar_height})"
        );

        let pixels_per_point = 1.0;
        let correct_bound =
            rows_content_bottom_y(reported_rows_top, row_count, ROW_HEIGHT, rows_area_height);
        // An image exactly as tall as the correctly-measured content: the
        // real Share crop must keep every pixel of it.
        let image_height_px = correct_bound.round() as usize;
        let correct_crop =
            screenshot_crop_height_px(correct_bound, pixels_per_point, image_height_px);
        assert_eq!(
            correct_crop, image_height_px,
            "the correct bound must keep the full image, not truncate real row pixels"
        );

        // The pre-#219 regression: computing the bound from `pre_call_top`
        // (before the bar and separator `draw_history` itself paints)
        // against that same image truncates real row content off the
        // bottom.
        let naive_bound =
            rows_content_bottom_y(pre_call_top, row_count, ROW_HEIGHT, rows_area_height);
        let naive_crop = screenshot_crop_height_px(naive_bound, pixels_per_point, image_height_px);
        assert!(
            naive_crop < image_height_px,
            "sanity check: the naive (pre-#219) bound must actually be \
             shorter, proving it would have cropped real row pixels off \
             (naive {naive_crop}, image {image_height_px})"
        );
    }
}
