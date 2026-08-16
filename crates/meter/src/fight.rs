//! Fight-boundary tracking: when the current fight ended, and how long its
//! stats stay frozen on screen afterwards (issue #78).
//!
//! This is deliberately separate from `reset.rs`. A *reset* clears the
//! displayed stats; a *fight end* does the opposite — it pins them, so the
//! last pull's numbers stay readable (and screenshottable) while the party
//! walks back to town. The clear only happens later, when real combat
//! activity starts the next fight (`ResetReason::NewFight`).

/// Where the meter is in the fight lifecycle. Derived from the encounter's
/// existing timestamps rather than stored redundantly — see
/// [`crate::encounter::Meter::fight_state`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FightState {
    /// No fight has started since the last reset: nothing to hold.
    #[default]
    Idle,
    /// A fight is in progress; stats accumulate and the elapsed timer runs.
    Active,
    /// The fight is over. Rows, totals, DPS and the elapsed timer are all
    /// frozen at the moment the fight ended, and stay that way until a
    /// manual reset, a server change, or the first damage of the next fight.
    Ended,
}

/// Tunables for fight-end detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FightConfig {
    /// A fight ends once this many milliseconds pass with no damage event.
    /// The 9s default sits above the longest realistic gap inside a pull
    /// (boss immunity phases, a wipe-recovery run-back is usually longer)
    /// while still freezing the meter quickly enough to screenshot.
    ///
    /// `0` disables idle detection entirely, leaving [`Self::end_on_boss_death`]
    /// as the only way a fight can end.
    pub idle_timeout_ms: u64,
    /// Whether a recognized boss dying (`DamageEvent::is_dead` on the current
    /// boss uid, or its HP reaching 0) ends the fight immediately instead of
    /// waiting out [`Self::idle_timeout_ms`]. Only fires for monster ids in
    /// `tables::BOSS_MONSTER_IDS`, so trash dying mid-pull can never end a
    /// fight early.
    pub end_on_boss_death: bool,
}

impl Default for FightConfig {
    fn default() -> Self {
        Self {
            idle_timeout_ms: 9_000,
            end_on_boss_death: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_idle_timeout_is_nine_seconds() {
        assert_eq!(FightConfig::default().idle_timeout_ms, 9_000);
    }

    #[test]
    fn boss_death_detection_is_on_by_default() {
        assert!(FightConfig::default().end_on_boss_death);
    }

    #[test]
    fn default_state_is_idle() {
        assert_eq!(FightState::default(), FightState::Idle);
    }
}
