//! Settings-driven side effects: update check, log and bundle export.

use super::*;

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
pub(crate) enum UpdateCheckState {
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
    /// Issue #250: the user clicked "Update now" and a spawned thread is
    /// downloading the release asset and swapping it in. `available` is the
    /// `CheckOutcome::UpdateAvailable` that offer came from, kept so the
    /// dropdown can keep naming the tag and can re-offer the same install
    /// if this one fails; `rx` carries the installed executable's path (to
    /// relaunch) or the reason it didn't get that far.
    Installing {
        available: CheckOutcome,
        rx: Receiver<Result<PathBuf, String>>,
    },
    /// Issue #250: the new build is on disk and has been started; this
    /// instance has already asked its viewport to close. A terminal state —
    /// the window is going away, and the only thing left to draw is a line
    /// saying why.
    Restarting,
    /// Issue #250: the download, the swap or the relaunch failed. Carries
    /// the original offer so the dropdown can redraw the "Update now"
    /// button beside the error and a retry costs one click.
    InstallFailed {
        available: CheckOutcome,
        error: String,
    },
}

/// What one `poll_update_check` drain found, before the borrow of
/// `OverlayApp::update_check` it was read through has ended. Exists purely
/// so the two channels (`Checking`'s and `Installing`'s) can be drained in
/// one match and acted on in another — assigning `self.update_check` inside
/// the first match would still be borrowing it.
pub(crate) enum LandedUpdate {
    Check(Result<CheckOutcome, String>),
    Install {
        available: CheckOutcome,
        result: Result<PathBuf, String>,
    },
}

/// The tag out of a `CheckOutcome::UpdateAvailable`, for the states that
/// carry one purely to name it. `UpToDate` has no tag and never reaches any
/// of those states, so it degrades to a neutral word rather than widening
/// every call site into a match.
pub(crate) fn update_tag(available: &CheckOutcome) -> &str {
    match available {
        CheckOutcome::UpdateAvailable { tag, .. } => tag,
        CheckOutcome::UpToDate => "the update",
    }
}

/// Draws the "an update exists, here's how to get it" row (issues #171 and
/// #250). Shared by the fresh-result state and the failed-install state so
/// a retry offers exactly the same affordances the first attempt did.
///
/// Two shapes, decided by whether the release actually published a
/// downloadable executable:
///
/// - With an asset (every release since issue #249): an "Update now" button
///   that downloads it, swaps it over this executable and relaunches, plus
///   a link to the release page for anyone who wants to read the notes
///   first.
/// - Without one — a release tagged before #249, which published a `.zip`,
///   or one whose upload never finished: the plain "Download" link that was
///   the whole affordance before #250. Offering an install button that
///   cannot work would be worse than offering the browser.
///
/// `egui::OpenUrl` (what `hyperlink_to` sends through `ctx.output_mut`) is
/// what eframe's native backend turns into an actual browser launch.
///
/// A click is reported through `clicked_install` rather than acted on here:
/// the caller holds the `&mut UpdateCheckState` this row was rendered from,
/// so the state change has to happen after this borrow ends.
pub(crate) fn draw_update_available(
    ui: &mut egui::Ui,
    available: &CheckOutcome,
    clicked_install: &mut Option<CheckOutcome>,
) {
    let CheckOutcome::UpdateAvailable {
        tag,
        url,
        asset_url,
    } = available
    else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(format!("Update available: {tag}"));
        match asset_url {
            Some(_) => {
                if ui.button("Update now").clicked() {
                    *clicked_install = Some(available.clone());
                }
                ui.hyperlink_to("Release notes", url.as_str());
            }
            None => {
                ui.hyperlink_to("Download", url.as_str());
            }
        }
    });
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
pub(crate) fn start_update_check() -> UpdateCheckState {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("update-check".to_string())
        .spawn(move || {
            let _ = tx.send(update_check::check_for_update(env!("CARGO_PKG_VERSION")));
        })
        .expect("failed to spawn the update-check thread");
    UpdateCheckState::Checking { rx }
}

/// Starts an in-place update (issue #250): spawns a one-shot `std::thread`
/// that calls `update_check::install_update` — the download, the "is this
/// really an executable" check, and the rename dance that puts it over the
/// running build — and sends the installed executable's path (or the
/// failure) back over a fresh `crossbeam_channel`. Returns the `Installing`
/// state `draw_header_menu`'s click handler stores on `OverlayApp` so
/// `poll_update_check` knows to start draining it.
///
/// Off the UI thread for a stronger reason than `start_update_check`'s: this
/// one pulls tens of megabytes over WinHTTP, so running it inline would
/// freeze the overlay — on top of the game, always visible — for the entire
/// download rather than for one request round-trip.
///
/// The thread deliberately stops at "the new file is in place". Relaunching
/// and closing the window are the UI thread's job
/// (`OverlayApp::finish_update_install`), since only it may send a viewport
/// command, and a background thread calling `std::process::exit` would tear
/// the app down mid-frame.
pub(crate) fn start_update_install(available: CheckOutcome) -> UpdateCheckState {
    let CheckOutcome::UpdateAvailable {
        asset_url: Some(asset_url),
        ..
    } = &available
    else {
        // Unreachable through the UI: `draw_update_available` only draws the
        // "Update now" button when there *is* an asset. Reported as a failed
        // install rather than panicked on, because an overlay that dies on a
        // menu click is worse than one that says it cannot do the thing.
        return UpdateCheckState::InstallFailed {
            available,
            error: "that release doesn't publish a downloadable executable".to_string(),
        };
    };
    let url = asset_url.clone();
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("update-install".to_string())
        .spawn(move || {
            let _ = tx.send(update_check::install_update(
                &url,
                env!("CARGO_PKG_VERSION"),
            ));
        })
        .expect("failed to spawn the update-install thread");
    UpdateCheckState::Installing { available, rx }
}

/// What one "Export logs" thread reports back (issue #220): the
/// destination it finished writing plus the bundle names it couldn't copy
/// (always empty for "Export logs", which has no per-entry reporting —
/// only "Export session bundle" can come up short, see
/// `bundle::export_bundle_to`), or that destination plus why it couldn't.
/// The destination rides along on the failure too so
/// `OverlayApp::poll_log_export` can name it in the log line — by the time
/// a reply lands, the click that chose it is long gone.
pub(crate) type LogExportOutcome = Result<(PathBuf, Vec<String>), (PathBuf, String)>;

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
pub(crate) fn start_log_export(dest: PathBuf, tx: Sender<LogExportOutcome>) {
    std::thread::Builder::new()
        .name("export-logs".to_string())
        .spawn(move || {
            let (log_path, _warning) = crate::logging::log_file_path();
            let outcome = match crate::logging::export_logs_to(&log_path, &dest) {
                Ok(()) => Ok((dest, Vec::new())),
                Err(err) => Err((dest, err.to_string())),
            };
            // A dropped receiver means the app is shutting down; the export
            // itself already happened, so there is nothing to report or
            // retry.
            let _ = tx.send(outcome);
        })
        .expect("failed to spawn the export-logs thread");
}

/// Starts a session-bundle export: spawns a one-shot `std::thread` that
/// gathers the log files, the packet-inspection dump ring (if
/// `SHINRA_INSPECT` was on this session), `settings.json`, and a
/// `manifest.json` into the directory `dest`, and sends the outcome back
/// over `tx` — the same `LogExportOutcome`/`poll_log_export` machinery
/// "Export logs" uses (see this module's own doc comment for why one
/// channel serves both items).
///
/// Every path is resolved *on the spawned thread*, not the click handler,
/// same as `start_log_export`: cheap either way, but keeping it off the
/// frame thread means a slow `APPDATA` lookup or a stalled disk (the copy
/// itself, `bundle::export_bundle_to`) can never stall a frame.
pub(crate) fn start_bundle_export(dest: PathBuf, tx: Sender<LogExportOutcome>) {
    std::thread::Builder::new()
        .name("export-bundle".to_string())
        .spawn(move || {
            let (log_path, _warning) = crate::logging::log_file_path();
            let log_parts = crate::logging::files_to_export(&log_path);

            let inspect_enabled = crate::inspect::enabled();
            let dump_parts = if inspect_enabled {
                bundle::dump_ring_parts(&crate::inspect::dump_path())
            } else {
                Vec::new()
            };

            let settings_path = crate::settings::settings_path();
            let entries = bundle::bundle_entries(&log_parts, &dump_parts, settings_path.as_deref());

            let session_id = crate::logging::session_id();
            let manifest = bundle::build_manifest(
                session_id,
                env!("CARGO_PKG_VERSION"),
                bundle::started_at_from_session_id(session_id),
                inspect_enabled,
                crate::dump::max_total_ring_bytes(),
                crate::inspect::dropped_count(),
            );

            let outcome = match bundle::export_bundle_to(&dest, &entries, &manifest) {
                Ok(missing) => Ok((dest, missing)),
                Err(err) => Err((dest, err.to_string())),
            };
            // A dropped receiver means the app is shutting down; the export
            // itself already happened, so there is nothing to report or
            // retry.
            let _ = tx.send(outcome);
        })
        .expect("failed to spawn the export-bundle thread");
}

/// The "Export logs" reply channel the header tests below hand to
/// `draw_header`/`draw_header_menu`. None of them click that item, so
/// nothing is ever sent over it — it exists only to satisfy the parameter,
/// and its dropped `Receiver` matters to nobody for the same reason.
#[cfg(test)]
pub(crate) fn unused_log_export_sender() -> Sender<LogExportOutcome> {
    crossbeam_channel::unbounded().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::*;
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
                &mut false,
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
                &mut false,
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

        app.poll_update_check(&egui::Context::default());

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

        app.poll_update_check(&egui::Context::default());

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

        app.poll_update_check(&egui::Context::default());

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
                    &mut false,
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
                &mut false,
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
                &mut false,
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

    /// Same shape as the test above, but for the update-available branch
    /// of a release that publishes no downloadable executable — every
    /// release tagged before issue #249 shipped a `.zip`, and issue #250's
    /// installer has nothing to install for one. That case must keep the
    /// pre-#250 affordance exactly: the tag, and a plain "Download" link to
    /// the release page. The actual `href` isn't a painted string at all
    /// (it's a `ViewportCommand::OpenUrl` queued on click, not text), so
    /// this only covers what a render test can see.
    #[test]
    fn draw_header_menu_falls_back_to_a_download_link_when_the_release_has_no_asset() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let mut update_check = UpdateCheckState::Done(Ok(CheckOutcome::UpdateAvailable {
            tag: "v0.3.0".to_string(),
            url: "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0".to_string(),
            asset_url: None,
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
                &mut false,
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
        assert!(
            !texts.contains(&"Update now".to_string()),
            "an install button must not be offered for a release with no asset, got {texts:?}"
        );
    }

    /// Issue #250's headline change: a release that publishes an executable
    /// offers to install it, and demotes the browser link to "Release
    /// notes" rather than dropping it — reading the notes before updating
    /// has to stay possible.
    #[test]
    fn draw_header_menu_offers_an_install_button_when_the_release_has_an_asset() {
        let texts = header_menu_texts(UpdateCheckState::Done(Ok(update_available(Some(
            "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/ShinraMeter-BPSR-v0.3.0-windows-x64.exe",
        )))));
        assert!(
            texts.contains(&"Update available: v0.3.0".to_string()),
            "expected the update-available line, got {texts:?}"
        );
        assert!(
            texts.contains(&"Update now".to_string()),
            "expected the install button, got {texts:?}"
        );
        assert!(
            texts.contains(&"Release notes".to_string()),
            "expected the release-notes link, got {texts:?}"
        );
    }

    /// The in-flight state has to name what it is doing; a dropdown that
    /// went blank mid-download would read as a crash.
    #[test]
    fn draw_header_menu_shows_the_download_in_progress() {
        let (_tx, rx) = crossbeam_channel::unbounded();
        let texts = header_menu_texts(UpdateCheckState::Installing {
            available: update_available(Some("https://github.com/x/y.exe")),
            rx,
        });
        assert!(
            texts.contains(&"Downloading v0.3.0…".to_string()),
            "expected the downloading line, got {texts:?}"
        );
    }

    #[test]
    fn draw_header_menu_shows_the_restart() {
        let texts = header_menu_texts(UpdateCheckState::Restarting);
        assert!(
            texts.contains(&"Restarting…".to_string()),
            "expected the restarting line, got {texts:?}"
        );
    }

    /// A failed install must say what went wrong *and* leave the retry one
    /// click away — a dropped connection is the common case, and making the
    /// user re-run the whole check first would be gratuitous.
    #[test]
    fn draw_header_menu_shows_a_failed_install_and_re_offers_it() {
        let texts = header_menu_texts(UpdateCheckState::InstallFailed {
            available: update_available(Some("https://github.com/x/y.exe")),
            error: "the connection was reset".to_string(),
        });
        assert!(
            texts.contains(&"Update failed: the connection was reset".to_string()),
            "expected the failure line, got {texts:?}"
        );
        assert!(
            texts.contains(&"Update now".to_string()),
            "expected the retry button, got {texts:?}"
        );
    }

    /// The "Check for updates" button is disabled while a check is in
    /// flight (issue #171); issue #250 extends that to the install and the
    /// restart, so a click cannot race the swap. `add_enabled(false, ..)`
    /// is not observable in painted text, so this drives the state machine
    /// instead: `start_update_install` on an offer with an asset must land
    /// in `Installing`, never `Done`.
    #[test]
    fn start_update_install_begins_in_the_installing_state() {
        assert!(matches!(
            start_update_install(update_available(Some(
                "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/app.exe"
            ))),
            UpdateCheckState::Installing { .. }
        ));
    }

    /// Defence in depth for the branch the UI never draws: an offer with no
    /// asset resolves to a stated failure rather than spawning a thread
    /// with nothing to download (or panicking on a menu click).
    #[test]
    fn start_update_install_refuses_an_offer_with_no_asset() {
        assert!(matches!(
            start_update_install(update_available(None)),
            UpdateCheckState::InstallFailed { .. }
        ));
    }

    /// The install thread's counterpart to
    /// `poll_update_check_reports_a_dead_thread_instead_of_hanging`: a
    /// dropped sender must resolve to a visible failure rather than leaving
    /// the dropdown on "Downloading…" with the button disabled forever.
    #[test]
    fn poll_update_check_reports_a_dead_install_thread() {
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
        let (tx, rx) = crossbeam_channel::unbounded::<Result<PathBuf, String>>();
        app.update_check = UpdateCheckState::Installing {
            available: update_available(Some("https://github.com/x/y.exe")),
            rx,
        };
        drop(tx);

        app.poll_update_check(&egui::Context::default());

        assert!(
            matches!(app.update_check, UpdateCheckState::InstallFailed { .. }),
            "a dead install thread must resolve to a failure, got {:?}",
            app.update_check
        );
    }

    /// A failed install keeps the offer it came from, so the retry button
    /// has the same asset URL the first attempt used.
    #[test]
    fn poll_update_check_keeps_the_offer_when_an_install_fails() {
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
        let offer = update_available(Some("https://github.com/x/y.exe"));
        let (tx, rx) = crossbeam_channel::unbounded::<Result<PathBuf, String>>();
        app.update_check = UpdateCheckState::Installing {
            available: offer.clone(),
            rx,
        };
        tx.send(Err("the connection was reset".to_string()))
            .unwrap();

        app.poll_update_check(&egui::Context::default());

        match &app.update_check {
            UpdateCheckState::InstallFailed { available, error } => {
                assert_eq!(available, &offer);
                assert_eq!(error, "the connection was reset");
            }
            other => panic!("expected InstallFailed, got {other:?}"),
        }
    }

    /// An install still in flight must stay `Installing` — the same
    /// "don't silently resolve to nothing" guarantee the check has.
    #[test]
    fn poll_update_check_leaves_an_in_flight_install_alone() {
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
        let (_tx, rx) = crossbeam_channel::unbounded::<Result<PathBuf, String>>();
        app.update_check = UpdateCheckState::Installing {
            available: update_available(Some("https://github.com/x/y.exe")),
            rx,
        };

        app.poll_update_check(&egui::Context::default());

        assert!(matches!(
            app.update_check,
            UpdateCheckState::Installing { .. }
        ));
    }

    /// `finish_update_install`'s relaunch-failure branch, exercised through
    /// `poll_update_check` exactly as production hits it: the install
    /// thread reports `Ok(path)`, and starting that path fails. A
    /// nonexistent path makes `Command::spawn` fail deterministically on
    /// any OS, so no seam is needed here — unlike the success branch below,
    /// which must not spawn a real process.
    #[test]
    fn poll_update_check_reports_a_relaunch_failure_after_a_successful_install() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let offer = update_available(Some("https://github.com/x/y.exe"));
        let (tx, rx) = crossbeam_channel::unbounded::<Result<PathBuf, String>>();
        app.update_check = UpdateCheckState::Installing {
            available: offer.clone(),
            rx,
        };
        tx.send(Ok(PathBuf::from(
            "/definitely/does/not/exist/ShinraMeter-BPSR.exe",
        )))
        .unwrap();

        app.poll_update_check(&egui::Context::default());

        match &app.update_check {
            UpdateCheckState::InstallFailed { available, error } => {
                assert_eq!(available, &offer);
                assert!(
                    error.contains("installed") && error.contains("couldn't be started"),
                    "expected a relaunch-failure message, got {error:?}"
                );
            }
            other => panic!("expected InstallFailed, got {other:?}"),
        }
        assert!(
            rx_command.try_recv().is_err(),
            "a relaunch failure must not tell the pipeline to quit — the old \
             process is still what's running"
        );
    }

    /// `finish_update_install`'s success branch: quit the pipeline, ask the
    /// viewport to close, and land on `Restarting`. Goes through
    /// `finish_update_install_with` (the seam added alongside this test)
    /// rather than `poll_update_check`, so the relaunch itself never spawns
    /// a real process — `poll_update_check`'s own plumbing into
    /// `finish_update_install` is unchanged production code and is not
    /// what's under test here.
    #[test]
    fn update_check_finish_install_relaunches_quits_and_closes_on_success() {
        let (_tx_snapshot, rx_snapshot) = crossbeam_channel::unbounded();
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut app = OverlayApp::new(
            rx_snapshot,
            tx_command,
            tx_settings,
            Settings::default(),
            None,
        );
        let offer = update_available(Some("https://github.com/x/y.exe"));
        let installed = PathBuf::from("/fake/relaunched/ShinraMeter-BPSR.exe");
        let ctx = egui::Context::default();

        let state = app.finish_update_install_with(&ctx, offer, Ok(installed.clone()), |exe| {
            assert_eq!(exe, installed.as_path());
            Ok(())
        });

        assert!(
            matches!(state, UpdateCheckState::Restarting),
            "expected Restarting, got {state:?}"
        );
        assert_eq!(
            rx_command
                .try_recv()
                .expect("a successful relaunch must send a command"),
            UiCommand::Quit
        );
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {});
        let close_commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        assert!(
            close_commands.contains(&egui::ViewportCommand::Close),
            "a successful relaunch must also ask the viewport to close: {close_commands:?}"
        );
        output.drop_without_applying_deltas();
        assert!(
            app.quit_requested,
            "a successful relaunch must set quit_requested (issue #321) so \
             drain_snapshots can tell the resulting pipeline-channel disconnect \
             apart from a dead pipeline"
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
        let mut quit_requested = false;

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
                &mut quit_requested,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let close_pos = accessible_rect_for_label(&update, "Close").center();
        layout.drop_without_applying_deltas();
        assert!(
            !quit_requested,
            "laying out the menu with no click must not itself request a quit"
        );

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
                &mut quit_requested,
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
        assert!(
            quit_requested,
            "clicking Close must set quit_requested (issue #321) so drain_snapshots \
             can tell the resulting pipeline-channel disconnect apart from a dead \
             pipeline"
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
        // Issue #233 lowered the default below `OPACITY_MAX`, so this can no
        // longer assume `Settings::default()` already sits at the ceiling —
        // it needs to start there explicitly for the drag-to-the-floor below
        // to prove it actually moved something.
        settings.set_opacity(Settings::OPACITY_MAX);
        assert_eq!(settings.opacity, Settings::OPACITY_MAX);

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
                &mut false,
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
                &mut false,
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
                &mut false,
            );
        });
        output.drop_without_applying_deltas();
        assert_eq!(settings.opacity, Settings::OPACITY_MIN);
        assert!(
            rx_settings.try_recv().is_err(),
            "releasing the slider without moving it must not send again"
        );
    }

    /// Issue #235: the opacity slider used to size itself off
    /// `Spacing::slider_width` (a fixed ~100pt rail) instead of stretching
    /// to fill its row the way the menu's other full-width controls do.
    /// Compares its painted rect against the "Minimize to tray" row's — a
    /// plain, always-visible control on the same root page (issue #120),
    /// so both are measured under the same width.
    ///
    /// Every other `draw_header_menu` test calls it directly on the bare
    /// `Ui` `ctx.run_ui` hands back, bypassing the real
    /// `egui::Popup::menu(&chevron_response)` wiring `draw_header` builds
    /// around it (see the doc comment on the Close-button regression test
    /// above). That's fine for click/drag behavior, but the "full width"
    /// this issue is about only exists because `Popup::menu` sets
    /// `Layout::top_down_justified` — outside that layout, *every* widget
    /// here (button included) just reports its own natural minimum size,
    /// so this test recreates that one piece of the real wiring by hand
    /// rather than the whole popup/anchor apparatus.
    #[test]
    fn draw_header_menu_opacity_slider_fills_the_row_width() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        let layout = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(220.0);
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
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
                    &mut false,
                );
            });
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let slider_rect = accessible_rect_for_role(&update, egui::accesskit::Role::Slider);
        let button_rect = accessible_rect_for_label(&update, "Minimize to tray");
        layout.drop_without_applying_deltas();

        // Issue #120 inset every control by `MENU_ROW_INSET` from the
        // popup's content edge, so "full width" now means the row's width
        // less that inset on each side rather than the row's width exactly.
        assert!(
            (slider_rect.left() - (button_rect.left() + MENU_ROW_INSET)).abs() < 1.0,
            "the slider must start on the same inset as the rows around it: \
             slider {slider_rect:?}, row {button_rect:?}"
        );
        assert!(
            (slider_rect.width() - (button_rect.width() - 2.0 * MENU_ROW_INSET)).abs() < 1.0,
            "the slider must fill the inset row width, not a fixed rail: \
             slider {slider_rect:?}, row {button_rect:?}"
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
                &mut false,
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
                &mut false,
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
            "Reset to defaults must restore the default opacity"
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

    /// Issue #121: the same button must now put back *every* customization,
    /// not just the opacity the test above covers — the custom header and
    /// backdrop images the issue names first, plus the visible-column set
    /// and the window size it names alongside them. Driven through the real
    /// `draw_header_menu` (not `Settings::reset_to_defaults` directly) so
    /// this also proves the button is wired to the wider reset at all, and
    /// that the persisted copy on `tx_settings` carries it.
    #[test]
    fn draw_header_menu_reset_to_defaults_clears_every_customization() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();
        // Every field issue #121 names, moved off its default first.
        let mut settings = Settings {
            visible_columns: vec![ColumnKind::Hits],
            window_size: Some([1234.0, 567.0]),
            ..Settings::default()
        };
        settings.set_background_image(ImageSlot::Header, Some(PathBuf::from("header.png")));
        settings.set_background_image(ImageSlot::Backdrop, Some(PathBuf::from("rows.png")));

        // Frame 1: lay the menu out and find where the button painted.
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
                &mut false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let reset_pos = accessible_rect_for_label(&update, "Reset to defaults").center();
        layout.drop_without_applying_deltas();

        // Frame 2: click it.
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
                &mut false,
            );
        });
        output.drop_without_applying_deltas();

        assert_eq!(
            settings,
            Settings::default(),
            "Reset to defaults must restore every user customization"
        );
        let sent = rx_settings
            .try_recv()
            .expect("Reset to defaults must send the updated settings on tx_settings");
        assert_eq!(
            sent,
            Settings::default(),
            "the persisted copy must carry the reset too"
        );
    }

    /// Issues #121/#253: the dropdown grows one Choose/Clear pair per
    /// customizable region, so a user can point the two at different files
    /// (or configure only one, which the issue is explicit about). Asserted
    /// on the *buttons* rather than the region labels beside them because
    /// egui puts only interactive widgets into the AccessKit tree — the
    /// plain `ui.label` rows, like the "Opacity" label above them, are not
    /// in it to look for.
    #[test]
    fn draw_header_menu_offers_a_row_for_every_background_image_slot() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, _rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

        // Issue #120 moved both rows onto the Background images *page*, so
        // this drives the real navigation rather than asserting on the
        // root: frame 1 locates the root row, frame 2 lets the click's
        // page change take effect, frame 3 reads the page back.
        let mut frame = |input: egui::RawInput| -> egui::accesskit::TreeUpdate {
            let output = ctx.run_ui(input, |ui| {
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
        };
        let update = frame(egui::RawInput::default());
        let row_pos = accessible_rect_for_label(&update, "Background images").center();
        let _ = frame(click_at(row_pos));
        let update = frame(egui::RawInput::default());
        let labels: Vec<String> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect();

        let expected = ImageSlot::ALL.len();
        for button in ["Choose…", "Clear"] {
            let found = labels.iter().filter(|label| *label == button).count();
            assert_eq!(
                found, expected,
                "expected one \"{button}\" per background-image slot, found {found}: {labels:?}"
            );
        }
        // The rows are told apart by their labels, so those must differ.
        assert_ne!(
            ImageSlot::Header.label(),
            ImageSlot::Backdrop.label(),
            "the two rows would be indistinguishable"
        );
    }

    /// Regression coverage for issue #93's fix and for issue #120's page
    /// structure, neither of which the direct-call tests above can see:
    /// they call `draw_header_menu` on a bare `Ui`, never through the
    /// `egui::Popup::menu(&chevron_response).close_behavior(CloseOnClickOutside)`
    /// wiring `draw_header` builds around it, so the `ui.close()` calls on
    /// the action rows are no-ops in that harness — a popup that was never
    /// opened has nothing to close.
    ///
    /// This drives the real thing through `draw_header` across many frames
    /// of the same `egui::Context` (memory persists across `run_ui` calls
    /// the way it would across real app frames): open the menu with a
    /// genuine click, drill into the Columns page, toggle a column, walk
    /// back out with the back row, and only then click Close. Every one of
    /// those is a *state* click that must leave the popup standing; Close
    /// is the one that must dismiss it, and must still dispatch Quit even
    /// with the real popup in the mix.
    ///
    /// It also pins issue #120's page-state rule at the other end:
    /// `draw_header` clears the remembered page on every frame the popup
    /// is shut, so reopening lands on the root rather than on whatever
    /// page the user was last looking at when they dismissed it.
    #[test]
    fn header_menu_popup_stays_open_for_a_column_checkbox_but_closes_for_close() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        // A zero animation time makes each click's effect fully painted on
        // the frame after it (egui's `animate_bool` snaps straight to the
        // target) instead of fading in over several — matching how
        // instantly a real click should feel anyway.
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();
        let snapshot = header_test_snapshot(0);
        let mut gesture = WindowGesture::default();
        let mut update_check = UpdateCheckState::default();

        // Runs one frame of the real `draw_header` (chevron, popup wiring,
        // and all) and hands back this frame's accessibility tree.
        let mut frame = |mut input: egui::RawInput| -> egui::accesskit::TreeUpdate {
            // A fixed, bounded screen every frame — the same reasoning as
            // `header_painted_boxes`'s doc comment: without one, the
            // popup's own best-alignment logic has no stable anchor to
            // measure against, and the chevron/menu paint at a different,
            // arbitrary offset each frame, silently invalidating a
            // position captured on an earlier frame.
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
        };
        let has_label = |update: &egui::accesskit::TreeUpdate, label: &str| {
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label().is_some_and(|s| s == label))
        };
        // "Columns" is the one label present on *every* page of the open
        // menu — the root's own drill-down row and the Columns page's back
        // row both carry it — and on none of the closed header, so it
        // answers "is the popup open" without assuming which page shows.
        let is_open = |update: &egui::accesskit::TreeUpdate| has_label(update, "Columns");

        // Frame 1: closed header, find the chevron.
        let update = frame(egui::RawInput::default());
        assert!(!is_open(&update), "the menu must start closed");
        let chevron_pos = accessible_rect_for_label(&update, "Menu").center();

        // Frame 2: click the chevron. `Popup::menu`'s `open_memory` toggles
        // and the popup paints in the very same frame (egui's `Popup::show`
        // opens before deciding whether to render), but its *position* is
        // not trustworthy yet: `Popup::show` runs the just-opened `Area`
        // through a `sizing_pass` with no prior measured size to align
        // against. So this frame only proves the menu opened.
        let update = frame(click_at(chevron_pos));
        assert!(is_open(&update), "clicking the chevron must open the menu");

        // Frame 3: no new input, just letting the popup settle into its
        // real, stable position now that a prior frame's size is on record.
        // This is the root page, so it carries the action rows and none of
        // the column checkboxes.
        let update = frame(egui::RawInput::default());
        assert!(is_open(&update), "the menu must still be open once settled");
        assert!(
            has_label(&update, "Restart packet capture"),
            "the root page must carry its own action rows"
        );
        let first_column_label = ColumnKind::ALL[0].label();
        assert!(
            !has_label(&update, first_column_label),
            "no column checkbox may sit on the root page"
        );
        let columns_pos = accessible_rect_for_label(&update, "Columns").center();

        // Frame 4: click the Columns row. It is a state row — no
        // `ui.close()`, per issue #120's rule — so the popup must survive
        // it; the page change it records lands on the next frame.
        let update = frame(click_at(columns_pos));
        assert!(
            is_open(&update),
            "opening the Columns page must not close the menu"
        );

        // Frame 5: the Columns page itself has replaced the root's body —
        // one popup, one page at a time, no second layer.
        let update = frame(egui::RawInput::default());
        assert!(is_open(&update), "the Columns page must still be the menu");
        assert!(
            has_label(&update, first_column_label),
            "the Columns page must list the columns"
        );
        assert!(
            !has_label(&update, "Restart packet capture"),
            "the root page's rows must be gone while a page is showing"
        );
        let checkbox_pos = accessible_rect_for_label(&update, first_column_label).center();

        // Frame 6: click a column checkbox. Issue #93's fix — no
        // `ui.close()` on this path, plus the root popup's
        // `CloseOnClickOutside` — means this must NOT dismiss the popup.
        let update = frame(click_at(checkbox_pos));
        assert!(
            is_open(&update),
            "a column checkbox click must leave the popup open"
        );
        assert!(
            rx_command.try_recv().is_err(),
            "a checkbox click must not dispatch a command"
        );
        let sent = rx_settings
            .try_recv()
            .expect("a column toggle must be handed to the settings writer");
        assert!(
            sent.is_visible(ColumnKind::ALL[0]),
            "the toggle must have enabled the column it was clicked on"
        );

        // Frame 7: still open on the frame after, too — not just within
        // the click frame itself. The back row (same "Columns" label, a
        // "◂" in the icon slot) is what walks out of the page.
        let update = frame(egui::RawInput::default());
        assert!(
            is_open(&update),
            "the popup must stay open on the frame after the checkbox click"
        );
        let back_pos = accessible_rect_for_label(&update, "Columns").center();

        // Frame 8: click Back. Also a state row, also no `ui.close()`.
        let _ = frame(click_at(back_pos));

        // Frame 9: back on the root, with the action rows returned.
        let update = frame(egui::RawInput::default());
        assert!(
            has_label(&update, "Restart packet capture"),
            "the back row must return to the root page"
        );
        let close_pos = accessible_rect_for_label(&update, "Close").center();

        // Frame 10: click Close. It calls `ui.close()` itself, so — unlike
        // every click above — this must dismiss the popup, and still
        // dispatch Quit through the real popup wiring.
        let _ = frame(click_at(close_pos));
        assert_eq!(
            rx_command.try_recv().expect("Close must send a command"),
            UiCommand::Quit
        );

        // Frame 11: `Ui::close` only closes for the *next* frame's
        // `is_open` check (the frame it's called on already painted before
        // the close decision runs) — so the popup must be gone by now.
        let update = frame(egui::RawInput::default());
        assert!(
            !is_open(&update),
            "Close must actually dismiss the popup by the following frame"
        );

        // Frames 12-13: reopen. Issue #120's page state is cleared by
        // `draw_header` on every frame the popup is shut, so this must
        // land on the root — not back on the Columns page the user was
        // drilled into two clicks ago.
        let _ = frame(click_at(chevron_pos));
        let update = frame(egui::RawInput::default());
        assert!(
            has_label(&update, "Restart packet capture"),
            "reopening the menu must land on the root page"
        );
        assert!(
            !has_label(&update, first_column_label),
            "reopening the menu must not restore the Columns page"
        );
    }

    /// Issue #214: `CaptureHandle::request_restart` shipped with no caller
    /// anywhere in this crate, so a capture wedge that no new TCP connection
    /// cleared (#211) left "relaunch the app" as the only recovery. This
    /// drives a real click on the dropdown item that now reaches it, the
    /// same way `draw_header_menu_dispatches_close_to_the_right_command`
    /// drives Close.
    #[test]
    fn draw_header_menu_dispatches_restart_packet_capture() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        apply_theme(&ctx);
        let icons = Icons::load(&ctx);
        let (tx_command, rx_command) = crossbeam_channel::unbounded();
        let (tx_settings, _rx_settings) = crossbeam_channel::unbounded();
        let mut settings = Settings::default();

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
                &mut false,
            );
        });
        let update = layout
            .platform_output
            .accesskit_update
            .clone()
            .expect("accesskit was enabled for this frame");
        let item_pos = accessible_rect_for_label(&update, "Restart packet capture").center();
        layout.drop_without_applying_deltas();

        let output = ctx.run_ui(click_at(item_pos), |ui| {
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
                &mut false,
            );
        });
        let viewport_commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();

        assert_eq!(
            rx_command
                .try_recv()
                .expect("Restart packet capture must send a command"),
            UiCommand::RestartCapture
        );
        assert!(
            !viewport_commands.contains(&egui::ViewportCommand::Close),
            "restarting capture must not close the overlay: {viewport_commands:?}"
        );
    }
}
