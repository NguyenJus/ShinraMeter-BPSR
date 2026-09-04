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

/// The cadence used while a background timer is in flight but nothing on
/// screen is actively moving — a transient status banner waiting to expire,
/// an update-check or history request waiting on its spawned thread's
/// reply. Matches the old unconditional constant, so none of those timers
/// got any less responsive by this change.
const TRANSIENT_TIMER_REPAINT: Duration = Duration::from_millis(100);

/// The fallback cadence once nothing above applies — slow enough that an
/// idle overlay costs essentially nothing, fast enough that the fight
/// clock (and any other once-a-second reading) never visibly stalls.
const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);

/// Everything [`repaint_policy`] needs to decide this frame's wakeup,
/// gathered by `OverlayApp::ui` from `self` and `ctx` before it calls
/// `ctx.request_repaint_after`.
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
    /// `Installing`), or a history list/load request in flight
    /// (`HistoryUi::pending`/`pending_load_id`). None of these animate —
    /// they are all "poll until a background thread's reply lands" — so
    /// they share the same cadence rather than each inventing its own.
    pub(crate) transient_timer_active: bool,
}

/// How soon `OverlayApp::ui` should ask egui to repaint, given this frame's
/// [`RepaintInputs`] — the argument to hand `ctx.request_repaint_after`
/// directly. Always returns `Some`: the [`IDLE_HEARTBEAT`] fallback means
/// the overlay never goes fully idle-forever, even with every input false,
/// so a caller cannot mistake `None` for "schedule nothing" by skipping the
/// call — there is no such case.
pub(crate) fn repaint_policy(inputs: RepaintInputs) -> Option<Duration> {
    let mut candidates: Vec<Duration> = Vec::new();
    if inputs.snapshot_activity {
        candidates.push(ACTIVITY_REPAINT);
    }
    if let Some(gif_wakeup) = inputs.gif_next_wakeup {
        candidates.push(gif_wakeup);
    }
    if inputs.input_active {
        candidates.push(ACTIVITY_REPAINT);
    }
    if inputs.transient_timer_active {
        candidates.push(TRANSIENT_TIMER_REPAINT);
    }
    Some(candidates.into_iter().min().unwrap_or(IDLE_HEARTBEAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing at all is happening: falls all the way back to the idle
    /// heartbeat, not to `None`/never-repaint.
    #[test]
    fn idle_falls_back_to_the_heartbeat() {
        let inputs = RepaintInputs::default();
        assert_eq!(repaint_policy(inputs), Some(IDLE_HEARTBEAT));
    }

    /// A snapshot just landed (or one is queued): repaint at the fast
    /// activity cadence, not the idle heartbeat.
    #[test]
    fn snapshot_activity_requests_the_fast_cadence() {
        let inputs = RepaintInputs {
            snapshot_activity: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), Some(ACTIVITY_REPAINT));
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
        assert_eq!(repaint_policy(inputs), Some(ACTIVITY_REPAINT));
    }

    /// A transient timer (status banner, update check, history request) is
    /// in flight: the mid cadence, not the fast one and not the heartbeat.
    #[test]
    fn transient_timer_requests_the_mid_cadence() {
        let inputs = RepaintInputs {
            transient_timer_active: true,
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), Some(TRANSIENT_TIMER_REPAINT));
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
        assert_eq!(repaint_policy(inputs), Some(delay));
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
        assert_eq!(repaint_policy(inputs), Some(delay));
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
        assert_eq!(repaint_policy(inputs), Some(ACTIVITY_REPAINT));
    }

    /// A `Duration::MAX` GIF wakeup (`animation_position_at`'s parked-on-
    /// frame-0 sentinel for a static image) must not win a `min()` against
    /// real candidates, but is still the answer when it is the only signal
    /// present — falling back to the heartbeat would repaint sooner than a
    /// static image ever needs, silently reintroducing idle cost.
    #[test]
    fn a_static_images_duration_max_wakeup_alone_is_returned_as_is() {
        let inputs = RepaintInputs {
            gif_next_wakeup: Some(Duration::MAX),
            ..RepaintInputs::default()
        };
        assert_eq!(repaint_policy(inputs), Some(Duration::MAX));
    }
}
