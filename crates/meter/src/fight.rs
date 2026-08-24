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

/// Why the current fight ended, for the `info`-level fight-end diagnostic
/// (issue #151's "Diagnostics gap": nothing was logged when a fight ended,
/// so the downstream `reset reason=NewFight` was the only clue in the log
/// and telling an idle-timeout end from a boss kill took in-game
/// confirmation).
///
/// Purely a diagnostic label — no meter behaviour branches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FightEndCause {
    /// A recognized boss died (see [`FightConfig::end_on_boss_death`]).
    BossDeath,
    /// [`FightConfig::idle_timeout_ms`] elapsed with no player damage.
    IdleTimeout,
    /// The party was down and the pull was over — every known member down
    /// at a player death (issue #154), or most of the roster down at the
    /// moment the boss's HP bar rolled back to full (issue #259).
    Wipe,
    /// The server session changed under a running fight — a reconnect, or
    /// a transition that re-issues uids (issue #138).
    ServerChanged,
    /// The player left the scene the fight was being fought in under a
    /// running fight (issue #191). Distinct from [`Self::ServerChanged`] on
    /// purpose: an ordinary same-shard dungeon transition is not a
    /// reconnect, and labelling it as one gives false hits to anyone
    /// grepping these lines for connection bugs.
    SceneChanged,
    /// The dungeon itself said the run is over — a `DungeonState::End` or
    /// `Settlement`, or a `DungeonVar { name: "IsFinishTarget" }` with a
    /// non-zero value (issue #139 §§3,7). More authoritative than a boss
    /// death or an idle timeout: the instance is telling the meter
    /// directly, rather than the meter inferring it from combat silence or
    /// a recognized monster id dying.
    DungeonEnded,
}

impl FightEndCause {
    /// Snake-case tag used in the log line, so causes stay greppable.
    pub fn label(self) -> &'static str {
        match self {
            Self::BossDeath => "boss_death",
            Self::IdleTimeout => "idle_timeout",
            Self::Wipe => "wipe",
            Self::ServerChanged => "server_changed",
            Self::SceneChanged => "scene_changed",
            Self::DungeonEnded => "dungeon_ended",
        }
    }
}

/// Tunables for fight-end detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FightConfig {
    /// A fight ends once this many milliseconds pass with no **player**
    /// damage (issue #155: a monster swinging at the party, or at their
    /// corpses, no longer counts).
    ///
    /// The 9s default is calibrated to freeze the meter quickly enough to
    /// screenshot, *not* to outlast every gap inside a pull — issue #151
    /// showed that no fixed value can, since a raid's immunity and mechanic
    /// windows exceed it by design. What makes 9s safe is that
    /// `Meter::fight_ended_at` suppresses this timeout entirely while the
    /// party is still in an instance with a damaged, living, recognized
    /// boss; the boundary there comes from the boss dying, the party wiping
    /// (issue #154) or leaving, never from the clock.
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
    /// How long after a boss-death fight end a hit on a **different phase of
    /// the same boss fight** (`crate::phase::same_phase_group`) resumes the
    /// held fight instead of clearing it and starting a new one (issue #124).
    ///
    /// A dungeon's final boss can fight through several phases, each a
    /// distinct monster id whose predecessor genuinely dies; the meter must
    /// keep accumulating across that. A raid's three sequential bosses are
    /// *not* in one phase group, so they still reset — this window only ever
    /// applies to a curated same-fight pair.
    ///
    /// 60s: generous enough for a phase-transition cutscene plus the
    /// invulnerability/positioning window before the next phase can be hit,
    /// and far shorter than the gap between two runs of the same dungeon.
    /// Without a bound, re-entering that dungeon an hour later and hitting
    /// the same boss family would silently graft the new pull onto the old
    /// one's frozen numbers.
    ///
    /// `0` disables phase resumption, restoring the pre-#124 behaviour where
    /// every post-hold hit starts a fresh fight.
    pub phase_resume_window_ms: u64,
}

impl Default for FightConfig {
    fn default() -> Self {
        Self {
            idle_timeout_ms: 9_000,
            end_on_boss_death: true,
            phase_resume_window_ms: 60_000,
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
    fn phase_resume_window_defaults_to_sixty_seconds() {
        assert_eq!(FightConfig::default().phase_resume_window_ms, 60_000);
    }

    #[test]
    fn every_fight_end_cause_has_its_own_label() {
        let labels = [
            FightEndCause::BossDeath.label(),
            FightEndCause::IdleTimeout.label(),
            FightEndCause::Wipe.label(),
            FightEndCause::ServerChanged.label(),
            FightEndCause::SceneChanged.label(),
            // issue #139: DungeonEnded joins the set this test guards.
            FightEndCause::DungeonEnded.label(),
        ];
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "labels must be distinguishable");
    }

    #[test]
    fn default_state_is_idle() {
        assert_eq!(FightState::default(), FightState::Idle);
    }
}
