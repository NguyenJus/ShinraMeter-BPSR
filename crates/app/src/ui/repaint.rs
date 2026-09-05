//! Issue #349: how often `OverlayApp::ui` asks egui to repaint.
//!
//! Before this module existed, `ui()` called
//! `ctx.request_repaint_after(Duration::from_millis(100))` unconditionally,
//! every single frame — a fixed 10 Hz clock that ran exactly as hot while
//! the overlay sat untouched over an idle game as it did while a fight was
//! in progress. [`repaint_policy`] replaces that constant with a decision:
//! repaint quickly while something is actually changing (a new snapshot,
//! live input, an animated background), and otherwise fall back to a slow
//! heartbeat so per-second timers (the transient status banner's expiry,
//! the fight clock once a snapshot does land) still tick without a
//! dedicated wakeup of their own.
//!
//! Pure and `egui`-free on purpose — `OverlayApp::ui` is the only caller
//! that gathers the real [`RepaintInputs`] from `self`/`ctx`, so the
//! decision table itself is testable without a live `egui::Context`.

use std::time::Duration;

/// The fast cadence used while something is actively happening this frame
/// (a snapshot just landed or one is queued up behind it, or the user's
/// mouse/keyboard produced input) — the same ~60 Hz a smooth drag or a
/// live-updating meter wants. Deliberately faster than the old unconditional
/// 100 ms: that constant was a ceiling chosen for the idle case, not a floor
/// for the active one.
const ACTIVITY_REPAINT: Duration = Duration::from_millis(16);

/// The cadence used while a background timer or an off-egui poll is in
/// flight but nothing on screen is actively moving: a transient status
/// banner waiting to expire, an update-check/history/log-export request
/// waiting on its spawned thread's reply, a Share screenshot capture
/// waiting on its `Event::Screenshot` (all of them
/// [`RepaintInputs::transient_timer_active`]), or click-through being on,
/// which makes this clock the only thing driving the per-frame cursor
/// hit-test and tray poll that can turn it back off
/// ([`RepaintInputs::click_through_active`]). Matches the old unconditional
/// constant, so none of those got any less responsive by this change.
const TRANSIENT_TIMER_REPAINT: Duration = Duration::from_millis(100);

/// The fallback cadence once nothing above applies — slow enough that an
/// idle overlay costs essentially nothing, fast enough that the fight
/// clock (and any other once-a-second reading) never visibly stalls.
const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);

/// Everything [`repaint_policy`] needs to decide this frame's wakeup,
/// gathered by `OverlayApp::ui` from `self` and `ctx` before it calls
/// `ctx.request_repaint_after`.
///
/// The full set of signals: fresh snapshot data, an animated GIF's own
/// next-frame deadline, live pointer/keyboard input, any background-thread
/// or frame-counted timer still in flight (status banner, update check,
/// history request, Share screenshot capture, log/bundle export), and
/// click-through being on. Anything not represented here cannot influence
/// the cadence, so a new "poll until a reply lands" feature has to be added
/// to one of these fields rather than requesting a repaint of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RepaintInputs {
    /// A new `Snapshot` was drained from `rx_snapshot` this frame, or the
    /// channel still holds one queued up for next frame — either way the
    /// pipeline thread is actively producing, so the overlay should keep
    /// pace with it rather than wait out the idle heartbeat.
    pub(crate) snapshot_activity: bool,
    /// The soonest an animated header/backdrop GIF wants its next frame
    /// painted (`custom_image::animation_position_at`'s `remaining`),
    /// or `None` if neither slot holds a live multi-frame entry. Also
    /// requested directly by `CustomImages::texture` the moment it advances
    /// a frame — folded in here too so the *decision* stays correct even if
    /// that call site ever stops requesting its own wakeup.
    pub(crate) gif_next_wakeup: Option<Duration>,
    /// Pointer or keyboard input landed this frame — a drag, a hover move,
    /// a click, a keystroke. Egui's own input state is the source of truth
    /// (see `OverlayApp::ui`'s call site), so a static hover with no
    /// pointer movement does not count: nothing about the pointer sitting
    /// still needs a fresh repaint to keep rendering correctly.
    pub(crate) input_active: bool,
    /// A transient UI timer is waiting to fire: the transient status
    /// banner's expiry (`status_expires_at`), a "Check for updates" or
    /// "Update now" request in flight (`UpdateCheckState::Checking`/
    /// `Installing`), a history list/load request in flight
    /// (`HistoryUi::pending`/`pending_load_id`), a Share screenshot capture
    /// waiting on its `Event::Screenshot` reply (`screenshot_capturing`,
    /// issue #350 O3), or a "Export logs"/"Export session bundle" copy
    /// thread waiting to report back (`log_exports_in_flight`, issue #350
    /// S2). None of these animate — they are all "poll until a background
    /// thread's reply lands, or a frame-counted guard times out" — so they
    /// share the same cadence rather than each inventing its own.
    pub(crate) transient_timer_active: bool,
    /// OS-level mouse click-through is on for this frame (`Settings::
    /// click_through`, not the platform-side atomic — see below). While
    /// true, `platform::click_through_passthrough_wanted`'s per-frame
    /// `GetCursorPos` hit-test against the toggle-cluster button, and
    /// `platform::take_tray_click_through_off_request`'s poll of the tray
    /// escape hatch, both receive no egui input at all (the window is
    /// passing every click straight through to the game underneath), so
    /// they depend entirely on this repaint clock to run at all — issue
    /// #350 O4.
    ///
    /// Deliberately sourced from `Settings::click_through` rather than the
    /// `CLICK_THROUGH_ENABLED` atomic `platform::click_through_passthrough_
    /// wanted` itself reads: the tray's "Turn off click-through" request
    /// clears that atomic immediately, on the spot, before `OverlayApp::ui`
    /// ever runs — see `click_through_after_tray_request`'s doc comment —
    /// so reading the atomic here would go false the same frame the atomic
    /// clears and stop scheduling the very repaint that lets `OverlayApp::
    /// ui` notice the tray request and paint the toggle-cluster button's
    /// now-off state. `Settings::click_through` instead only flips one
    /// frame later, once `OverlayApp::ui` has reconciled it, so this stays
    /// true for that whole frame too.
    pub(crate) click_through_active: bool,
}

/// How soon `OverlayApp::ui` should ask egui to repaint, given this frame's
/// [`RepaintInputs`] — the argument to hand `ctx.request_repaint_after`
/// directly. Returns a plain `Duration` rather than an `Option`: the
/// [`IDLE_HEARTBEAT`] fallback means the overlay never goes fully
/// idle-forever, even with every input false, so there is no
/// "schedule nothing" case for a caller to have to handle (or to skip the
/// call for). Every candidate — the [`IDLE_HEARTBEAT`] included — is
/// clamped to the heartbeat, which is also what keeps
/// `animation_position_at`'s [`Duration::MAX`] static-image sentinel out of
/// eframe's unchecked `Instant::now() + delay`.
pub(crate) fn repaint_policy(inputs: RepaintInputs) -> Duration {
    let mut soonest: Option<Duration> = None;
    let mut consider = |candidate: Duration| {
        soonest = Some(soonest.map_or(candidate, |current: Duration| current.min(candidate)));
    };
    if inputs.snapshot_activity || inputs.input_active {
        consider(ACTIVITY_REPAINT);
    }
    if let Some(gif_wakeup) = inputs.gif_next_wakeup {
        consider(gif_wakeup);
    }
    if inputs.transient_timer_active || inputs.click_through_active {
        consider(TRANSIENT_TIMER_REPAINT);
    }
    soonest.map_or(IDLE_HEARTBEAT, |d| d.min(IDLE_HEARTBEAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing at all is happening: falls all the way back to the idle
    /// heartbeat, not to `None`/never-repaint.
    #[test]
    fn idle_falls_back_to_the_heartbeat() {
        let inputs = RepaintInputs::default();
        assert_eq!(repaint_policy(inputs), IDLE_HEARTBEAT);
    }

    /// A snapshot just landed (or one is queued): repaint at the fast
    /// activity cadence, not the idle heartbeat.
    #[test]
    fn snapshot_activity_requests_the_fast_cadence() {
        let inputs = RepaintInputs {
            snapshot_activity: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), ACTIVITY_REPAINT);
    }

    /// Live pointer/keyboard input this frame: same fast cadence as fresh
    /// snapshot data, so a drag or hover feels exactly as smooth as it did
    /// under the old unconditional 100 ms clock.
    #[test]
    fn input_activity_requests_the_fast_cadence() {
        let inputs = RepaintInputs {
            input_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), ACTIVITY_REPAINT);
    }

    /// A transient timer (status banner, update check, history request) is
    /// in flight: the mid cadence, not the fast one and not the heartbeat.
    #[test]
    fn transient_timer_requests_the_mid_cadence() {
        let inputs = RepaintInputs {
            transient_timer_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), TRANSIENT_TIMER_REPAINT);
    }

    /// An animated background GIF reports its own next-frame delay: that
    /// exact duration is what gets requested, not a rounded-up bucket.
    #[test]
    fn gif_wakeup_is_requested_verbatim() {
        let delay = Duration::from_millis(37);
        let inputs = RepaintInputs {
            gif_next_wakeup: Some(delay),
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), delay);
    }

    /// A GIF due sooner than the fast activity cadence must not be
    /// stretched out to it — the frame timing is authoritative for how
    /// smooth the animation looks.
    #[test]
    fn a_gif_wakeup_faster_than_activity_cadence_is_not_slowed_down() {
        let delay = Duration::from_millis(4);
        let inputs = RepaintInputs {
            snapshot_activity: true,
            gif_next_wakeup: Some(delay),
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), delay);
    }

    /// Several signals firing the same frame take the soonest of them —
    /// never the heartbeat, and never a slower one just because it was
    /// listed first.
    #[test]
    fn multiple_signals_take_the_soonest() {
        let inputs = RepaintInputs {
            snapshot_activity: true,
            transient_timer_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), ACTIVITY_REPAINT);
    }

    /// A `Duration::MAX` GIF wakeup (`animation_position_at`'s parked-on-
    /// frame-0 sentinel for a static image) must not win a `min()` against
    /// real candidates, and must not be handed to eframe's own unchecked
    /// `Instant::now() + delay` either — it is clamped down to the idle
    /// heartbeat like every other candidate.
    #[test]
    fn a_static_images_duration_max_wakeup_alone_is_clamped_to_the_heartbeat() {
        let inputs = RepaintInputs {
            gif_next_wakeup: Some(Duration::MAX),
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), IDLE_HEARTBEAT);
    }

    /// Issue #350 (O4): click-through on is on its own enough to hold the
    /// mid cadence — while it is on, the window gets no egui input at all,
    /// so this clock is the only thing running
    /// `platform::click_through_passthrough_wanted`'s cursor hit-test and
    /// the tray's "turn it off" poll.
    #[test]
    fn click_through_alone_requests_the_mid_cadence() {
        let inputs = RepaintInputs {
            click_through_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), TRANSIENT_TIMER_REPAINT);
    }

    /// Click-through never *slows* a frame that already has real activity:
    /// it raises a floor, it does not replace the faster candidates.
    #[test]
    fn click_through_does_not_slow_an_active_frame() {
        let inputs = RepaintInputs {
            click_through_active: true,
            input_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), ACTIVITY_REPAINT);
    }

    /// A GIF wakeup slower than the idle heartbeat (e.g. a 2s delay between
    /// frames) is clamped down to the heartbeat rather than letting a single
    /// slow animation replace the overlay's own once-a-second timers for
    /// that long.
    #[test]
    fn a_gif_wakeup_slower_than_the_heartbeat_is_clamped_to_it() {
        let inputs = RepaintInputs {
            gif_next_wakeup: Some(Duration::from_secs(2)),
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), IDLE_HEARTBEAT);
    }
}
