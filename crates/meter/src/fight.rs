//! Fight-boundary tracking: when the current fight ended, and how long its
//! stats stay frozen on screen afterwards (issue #78).
//!
//! This is deliberately separate from `reset.rs`. A *reset* clears the
//! displayed stats; a *fight end* does the opposite — it pins them, so the
//! last pull's numbers stay readable (and screenshottable) while the party
//! walks back to town. The clear only happens later, when real combat
//! activity starts the next fight (`ResetReason::NewFight`).
//!
//! [`FightLifecycle`] is the stored state machine that decides all of
//! this (issue #336 step 3): `crate::encounter::Meter` keeps exactly one
//! of them, and every fight boundary — start, end, phase resume, wipe
//! hold, reset — is one of its transitions. [`FightState`] and
//! [`Lifecycle`] are the read-only, `now_ms`-relative views layered on
//! top, since the idle-timeout end is derived rather than latched at the
//! instant it becomes true.

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

/// Which hold, if any, is keeping an ended fight's events withheld beyond
/// the ordinary [`FightState::Ended`] freeze (issue #336). Only one hold
/// exists today — a party wipe
/// (`crate::encounter::Meter::withholds_after_wipe`, issue #154) — split
/// into its own type rather than a bare bool so a caller that already
/// matches on this does not have to change shape if a second kind is ever
/// added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HoldKind {
    /// The whole party went down and the attempt is being held open for a
    /// possible re-pull, per
    /// `crate::encounter::Meter::withholds_after_wipe`.
    Wipe,
}

/// Read-only, point-in-time view of the fight lifecycle (issue #336 step
/// 1). Every value here is derived on the fly from state `Meter` already
/// stores — nothing new is retained to produce it, so this changes no
/// behaviour. It's the accessor surface a later, explicit state machine
/// (issue #336 step 3) replaces the derivation behind, without callers
/// having to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// No fight has started since the last reset.
    Idle,
    /// A fight is in progress, started at `since_ms`.
    Active { since_ms: u64 },
    /// The fight is over and its stats are frozen as of `at_ms`. `cause` is
    /// `Some` for every fight end (issue #336 step 2 stores the
    /// `FightEndCause` `latch_fight_end` is given, alongside the timestamp
    /// fields it always latched) — `None` only when no fight has ended
    /// since the last reset, which this arm is never reached for anyway.
    Ended {
        at_ms: u64,
        cause: Option<FightEndCause>,
    },
    /// The fight ended in a party wipe and the attempt is still being held
    /// open for a possible re-pull, per
    /// `crate::encounter::Meter::withholds_after_wipe`. A refinement of
    /// `Ended` for callers that care about the hold specifically, not a
    /// disjoint state — `fight_end_cause` reports `Wipe` here too.
    Held { kind: HoldKind, since_ms: u64 },
}

/// The encounter's **stored** fight lifecycle (issue #336 step 3) — the one
/// place `crate::encounter::Meter` keeps "where is this fight", replacing
/// the six loosely-coupled fields (`fight_start_ms`, `fight_end_ms`,
/// `fight_end_observed_ms`, `fight_end_boss_id`, `fight_end_cause`,
/// `wipe_hold`) it used to derive that answer from.
///
/// Modelled on the reference implementation's `BattleStateMachine`
/// (`BPSR-ZDPS`), which stores the state and transitions it explicitly
/// rather than re-deriving it from timestamps on every read. The six fields
/// could spell out combinations no code path could ever produce — an end
/// time with no start, a phase-resume arming with no end, a wipe hold on a
/// fight that never ended — and every guard in `encounter.rs` had to
/// re-establish by hand that it wasn't looking at one. The enum makes the
/// reachable set the *only* set: each arm carries exactly the data that
/// state has, and the transitions below are the only way to move between
/// them.
///
/// Not to be confused with [`Lifecycle`], the read-only, `now_ms`-relative
/// view callers get: that one still folds in the *derived* idle-timeout end
/// (`crate::encounter::Meter::fight_state`), which by design is visible
/// before anything latches it. This type holds only what has actually been
/// latched.
///
/// Legal transitions:
///
/// ```text
///                start(ms)               end(..)
///   Idle ────────────────────▶ Active ────────────▶ Ended
///     ▲                          ▲                    │
///     │                          └──── resume() ──────┤
///     └──────────── reset(), from any state ──────────┘
/// ```
///
/// plus four in-place refinements of an existing state:
/// [`Self::hold`]/[`Self::release_hold`] (the issue #154 wipe hold) and
/// [`Self::arm_phase_resume`]/[`Self::disarm_phase_resume`] (issue
/// #124/#316 phase resumption).
///
/// Anything else is a no-op that logs at `debug` — never a panic. A meter
/// fed a truncated or out-of-order capture must keep running, and the old
/// unconditional field stores would silently produce a nonsense combination
/// in exactly those cases; refusing the write and saying so is strictly
/// more informative and never less safe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FightLifecycle {
    /// No fight has started since the last reset. Carries nothing: the only
    /// way into this state is [`Self::reset`], which drops everything.
    #[default]
    Idle,
    /// A fight is in progress.
    Active {
        /// When the fight's first damage landed — the old `fight_start_ms`.
        /// Stable for the whole life of one fight, including across a
        /// [`Self::resume`] (issue #124).
        start_ms: u64,
        /// A hold carried in from the [`Self::Ended`] state a
        /// [`Self::resume`] came out of.
        ///
        /// **Unreachable in practice**, and modelled anyway. The old code's
        /// phase-resume branch cleared the four fight-end fields and
        /// deliberately left `wipe_hold` alone, so "resumed while held" was
        /// a state it could spell. That it never actually occurred is a
        /// two-step argument (a `Wipe` latch never arms
        /// `fight_end_boss_id`, and phase resumption requires that arming),
        /// not something either the fields or the call sites enforced.
        /// Dropping the hold here would be a silent behaviour change
        /// resting entirely on that argument staying true, so the state is
        /// carried faithfully instead.
        hold: Option<HoldKind>,
    },
    /// The fight is over and its stats are frozen (issue #78).
    Ended {
        /// The fight's start, preserved so a [`Self::resume`] can restore
        /// it and so the frozen elapsed timer keeps a denominator.
        start_ms: u64,
        /// When the fight ended — the old `fight_end_ms`. For an
        /// [`FightEndCause::IdleTimeout`] end this is the last *player*
        /// hit, not "now".
        end_ms: u64,
        /// When the end was actually latched — the old
        /// `fight_end_observed_ms` (issue #316). Equal to `end_ms` for
        /// every cause but [`FightEndCause::IdleTimeout`], whose end can be
        /// a boss-engagement window in the past by the time
        /// `crate::encounter::Meter`'s idle suppression lets the timeout
        /// through. The phase-resume window is measured from this, not from
        /// `end_ms`.
        observed_ms: u64,
        /// Why the fight ended — the old `fight_end_cause` (issue #336 step
        /// 2). Not an `Option` any more: reaching this state *is* having a
        /// cause, which is precisely the invariant the separate field could
        /// not express.
        cause: FightEndCause,
        /// The monster id whose death arms phase resumption, if this end
        /// armed it — the old `fight_end_boss_id` (issue #124/#316). `None`
        /// for a cause that arms nothing, and cleared by
        /// [`Self::disarm_phase_resume`] when the entity map it would be
        /// matched against is dropped.
        boss_id: Option<u32>,
        /// The wipe hold, if this end was a party wipe still being held
        /// open for a re-pull (issue #154) — the old `wipe_hold` flag.
        ///
        /// A refinement of `Ended`, not a state beside it: the old flag
        /// only ever went up alongside a fight-end latch, and every read of
        /// it already presupposed an ended fight.
        hold: Option<HoldKind>,
    },
}

impl FightLifecycle {
    /// When the current (running or held) fight started — the old
    /// `Meter::fight_start_ms` field read.
    pub fn start_ms(self) -> Option<u64> {
        match self {
            Self::Idle => None,
            Self::Active { start_ms, .. } | Self::Ended { start_ms, .. } => Some(start_ms),
        }
    }

    /// When the currently-held fight ended — the old `Meter::fight_end_ms`
    /// field read. `None` unless [`Self::Ended`].
    pub fn end_ms(self) -> Option<u64> {
        match self {
            Self::Ended { end_ms, .. } => Some(end_ms),
            _ => None,
        }
    }

    /// When the currently-held fight's end was *observed* — the old
    /// `Meter::fight_end_observed_ms` field read.
    pub fn end_observed_ms(self) -> Option<u64> {
        match self {
            Self::Ended { observed_ms, .. } => Some(observed_ms),
            _ => None,
        }
    }

    /// Why the currently-held fight ended — the old
    /// `Meter::fight_end_cause` field read.
    pub fn end_cause(self) -> Option<FightEndCause> {
        match self {
            Self::Ended { cause, .. } => Some(cause),
            _ => None,
        }
    }

    /// The monster id phase resumption is armed against — the old
    /// `Meter::fight_end_boss_id` field read.
    pub fn phase_resume_boss_id(self) -> Option<u32> {
        match self {
            Self::Ended { boss_id, .. } => boss_id,
            _ => None,
        }
    }

    /// Which hold, if any, is in force — the old `Meter::wipe_hold` flag
    /// read, widened to a kind (issue #336 step 2).
    pub fn hold_kind(self) -> Option<HoldKind> {
        match self {
            Self::Idle => None,
            Self::Active { hold, .. } | Self::Ended { hold, .. } => hold,
        }
    }

    /// Clears everything: back to [`Self::Idle`]. Legal from every state —
    /// this is `Meter::reset`, which every reset reason reaches.
    pub fn reset(&mut self) -> bool {
        *self = Self::Idle;
        true
    }

    /// [`Self::Idle`] → [`Self::Active`]: the first player damage of a new
    /// fight landed at `start_ms`.
    pub fn start(&mut self, start_ms: u64) -> bool {
        match *self {
            Self::Idle => {
                *self = Self::Active {
                    start_ms,
                    hold: None,
                };
                true
            }
            other => {
                log::debug!("fight lifecycle: start({start_ms}) refused in {other:?}");
                false
            }
        }
    }

    /// [`Self::Active`] → [`Self::Ended`]: the fight ended at `end_ms`, as
    /// observed at `observed_ms`, for `cause`, arming phase resumption
    /// against `boss_id` (`None` for a cause that arms nothing).
    ///
    /// Refusing an `end` on an already-[`Self::Ended`] fight is
    /// `Meter::latch_fight_end`'s idempotence guard ("a fight already
    /// latched returns untouched") expressed in the type: the repeated "pin
    /// the end" calls in `Meter::apply_damage` and `Meter::tick` rely on it.
    pub fn end(
        &mut self,
        end_ms: u64,
        observed_ms: u64,
        cause: FightEndCause,
        boss_id: Option<u32>,
    ) -> bool {
        match *self {
            Self::Active { start_ms, hold } => {
                *self = Self::Ended {
                    start_ms,
                    end_ms,
                    observed_ms,
                    cause,
                    boss_id,
                    hold,
                };
                true
            }
            other => {
                log::debug!(
                    "fight lifecycle: end(cause={}) refused in {other:?}",
                    cause.label()
                );
                false
            }
        }
    }

    /// [`Self::Ended`] → [`Self::Active`], keeping `start_ms` (and any
    /// hold): the held fight's next phase was hit inside
    /// `FightConfig::phase_resume_window_ms` (issue #124), so this is the
    /// same encounter continuing rather than a new one.
    pub fn resume(&mut self) -> bool {
        match *self {
            Self::Ended { start_ms, hold, .. } => {
                *self = Self::Active { start_ms, hold };
                true
            }
            other => {
                log::debug!("fight lifecycle: resume() refused in {other:?}");
                false
            }
        }
    }

    /// Puts an ended fight under `kind`'s hold (issue #154's party wipe is
    /// the only one today). Legal only on an [`Self::Ended`] fight that is
    /// not already held — the old flag only ever went up immediately after
    /// a `Wipe` latch that actually took.
    pub fn hold(&mut self, kind: HoldKind) -> bool {
        match self {
            Self::Ended {
                hold: slot @ None, ..
            } => {
                *slot = Some(kind);
                true
            }
            other => {
                log::debug!("fight lifecycle: hold({kind:?}) refused in {other:?}");
                false
            }
        }
    }

    /// Lifts whatever hold is in force, leaving the rest of the state
    /// alone. Returns `false` when there was nothing held — not an illegal
    /// transition, just nothing to do, so it stays silent: the call sites
    /// (leaving an instance, a server change, `Meter::reset`) drop the hold
    /// unconditionally and are reached far more often with no hold than
    /// with one.
    pub fn release_hold(&mut self) -> bool {
        match self {
            Self::Idle => false,
            Self::Active { hold, .. } | Self::Ended { hold, .. } => hold.take().is_some(),
        }
    }

    /// Arms phase resumption against `boss_id` on an already-ended fight —
    /// `Meter::end_fight_on_boss_death`'s follow-up to its own latch, which
    /// records the *dying* boss's id rather than whichever one
    /// `recompute_boss` may since have moved onto (issue #210/#211).
    pub fn arm_phase_resume(&mut self, boss_id: Option<u32>) -> bool {
        match self {
            Self::Ended { boss_id: armed, .. } => {
                *armed = boss_id;
                true
            }
            other => {
                log::debug!("fight lifecycle: arm_phase_resume({boss_id:?}) refused in {other:?}");
                false
            }
        }
    }

    /// Disarms phase resumption, leaving the rest of the state alone: the
    /// `enemies` map a resume candidate would be looked up in has just been
    /// cleared (issue #316's dungeon-entry and `ServerChanged` paths), so a
    /// stale arming would withhold every subsequent hit as undecided.
    /// Silent when nothing was armed, for the same reason
    /// [`Self::release_hold`] is.
    pub fn disarm_phase_resume(&mut self) -> bool {
        match self {
            Self::Ended { boss_id, .. } => boss_id.take().is_some(),
            _ => false,
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
    /// `Meter::fight_ended_at` suppresses this timeout entirely while there
    /// is a damaged, living, recognized boss the party was hitting recently
    /// (`Meter::engaged_boss_still_up`); the boundary there comes from the
    /// boss dying, the party wiping (issue #154), the scene changing (issue
    /// #191) or the engagement window lapsing, never from this clock.
    ///
    /// Issue #313: that suppression used to apply only inside a
    /// `tables::DUNGEON_SCENE_IDS` instance, which left world-boss arenas
    /// (scene 7152) running on the bare 9s and wiping live pulls the moment
    /// a boss went invulnerable. It is scene-independent now, bounded
    /// instead by `BOSS_ENGAGEMENT_WINDOW_MS`.
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
    /// How long after a fight ends (`fight_end_ms`) a trailing event is
    /// still folded into that ended fight's *stats* rather than either
    /// resetting into a new fight or being dropped outright.
    ///
    /// Modeled on the reference implementation's fight-end deferral
    /// (`BPSR-ZDPS/BattleStateMachine.cs`'s "new data will still be applied
    /// to this ended encounter" and `EncounterManager.cs`'s 2s/5s final-end
    /// delay, "because some packets are going to be delayed and come in
    /// after this and they are typically the most important ones"): a
    /// boss's last DoT ticks, a killing-blow retransmit, and a buff's
    /// closing `Remove` all routinely arrive a few hundred milliseconds
    /// after the packet that actually latched `fight_end_ms`, and without
    /// this window every one of them used to be discarded — undercounting
    /// totals and buff uptime on the tail of *every* kill.
    ///
    /// Distinct from [`Self::phase_resume_window_ms`]: a phase resume
    /// un-freezes the fight (clears `fight_end_ms`, keeps the clock
    /// running) because the pull is provably still going. This window does
    /// the opposite on purpose — the fight stays frozen exactly as it was
    /// (`fight_end_ms`, the elapsed timer, and `FightState::Ended` are all
    /// left untouched) and only the accumulated stats grow, since a
    /// straggling packet is evidence the fight *just* ended, not that it
    /// didn't.
    ///
    /// 2000ms, matching the reference implementation's "known final"
    /// deferral — generous enough for ordinary reassembly/decode jitter on
    /// the last few packets of a kill, far short of
    /// [`Self::phase_resume_window_ms`] so it can never be mistaken for one
    /// (a phase-group hit inside this window still resumes the fight via
    /// [`Self::phase_resume_window_ms`]'s own, unrelated check; a
    /// non-phase-group hit inside this window is what this field is for).
    ///
    /// `0` disables the grace window entirely, restoring the pre-fix
    /// behaviour where every event after `fight_end_ms` is either a new
    /// fight or dropped.
    pub post_end_grace_ms: u64,
}

impl Default for FightConfig {
    fn default() -> Self {
        Self {
            idle_timeout_ms: 9_000,
            end_on_boss_death: true,
            phase_resume_window_ms: 60_000,
            post_end_grace_ms: 2_000,
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
    fn post_end_grace_defaults_to_two_seconds() {
        assert_eq!(FightConfig::default().post_end_grace_ms, 2_000);
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

    /// Issue #336 step 3: one test per transition of the stored
    /// [`FightLifecycle`] state machine, including every refusal. The
    /// refusals are the point — the six fields this replaces accepted every
    /// write unconditionally, so "an end with no start" or "a hold on a
    /// live fight" were states the old code could reach and no test could
    /// name.
    mod lifecycle_state_machine {
        use super::*;

        fn ended() -> FightLifecycle {
            let mut lc = FightLifecycle::Idle;
            assert!(lc.start(100));
            assert!(lc.end(900, 1_000, FightEndCause::BossDeath, Some(103)));
            lc
        }

        #[test]
        fn a_fresh_lifecycle_is_idle_and_knows_nothing() {
            let lc = FightLifecycle::default();
            assert_eq!(lc, FightLifecycle::Idle);
            assert_eq!(lc.start_ms(), None);
            assert_eq!(lc.end_ms(), None);
            assert_eq!(lc.end_observed_ms(), None);
            assert_eq!(lc.end_cause(), None);
            assert_eq!(lc.phase_resume_boss_id(), None);
            assert_eq!(lc.hold_kind(), None);
        }

        #[test]
        fn start_moves_idle_to_active() {
            let mut lc = FightLifecycle::Idle;
            assert!(lc.start(100));
            assert_eq!(
                lc,
                FightLifecycle::Active {
                    start_ms: 100,
                    hold: None
                }
            );
            assert_eq!(lc.start_ms(), Some(100));
            assert_eq!(lc.end_ms(), None);
        }

        #[test]
        fn start_is_refused_on_a_running_fight() {
            let mut lc = FightLifecycle::Idle;
            assert!(lc.start(100));
            assert!(!lc.start(200), "the clock must not move mid-fight");
            assert_eq!(lc.start_ms(), Some(100));
        }

        #[test]
        fn start_is_refused_on_an_ended_fight() {
            let mut lc = ended();
            let before = lc;
            assert!(!lc.start(2_000));
            assert_eq!(lc, before, "a held fight is not restarted in place");
        }

        #[test]
        fn end_moves_active_to_ended_and_records_every_field() {
            let mut lc = FightLifecycle::Idle;
            assert!(lc.start(100));
            assert!(lc.end(900, 1_000, FightEndCause::IdleTimeout, Some(103)));
            assert_eq!(lc.start_ms(), Some(100));
            assert_eq!(lc.end_ms(), Some(900));
            assert_eq!(lc.end_observed_ms(), Some(1_000));
            assert_eq!(lc.end_cause(), Some(FightEndCause::IdleTimeout));
            assert_eq!(lc.phase_resume_boss_id(), Some(103));
            assert_eq!(lc.hold_kind(), None);
        }

        #[test]
        fn end_is_refused_when_no_fight_is_running() {
            let mut lc = FightLifecycle::Idle;
            assert!(!lc.end(900, 900, FightEndCause::BossDeath, None));
            assert_eq!(lc, FightLifecycle::Idle, "no end without a start");
        }

        #[test]
        fn end_is_refused_on_an_already_ended_fight() {
            // `Meter::latch_fight_end`'s idempotence guard, in the type:
            // the repeated "pin the end" calls must not re-stamp a latch.
            let mut lc = ended();
            assert!(!lc.end(5_000, 5_000, FightEndCause::IdleTimeout, None));
            assert_eq!(lc.end_ms(), Some(900));
            assert_eq!(lc.end_cause(), Some(FightEndCause::BossDeath));
            assert_eq!(lc.phase_resume_boss_id(), Some(103));
        }

        #[test]
        fn resume_moves_ended_back_to_active_keeping_the_start() {
            let mut lc = ended();
            assert!(lc.resume());
            assert_eq!(
                lc,
                FightLifecycle::Active {
                    start_ms: 100,
                    hold: None
                },
                "issue #124: one encounter throughout"
            );
            assert_eq!(lc.end_ms(), None);
            assert_eq!(lc.end_cause(), None);
            assert_eq!(lc.phase_resume_boss_id(), None);
        }

        #[test]
        fn resume_carries_a_hold_across_the_resumption() {
            // Unreachable in the meter today, and preserved deliberately —
            // the old phase-resume branch cleared the four end fields and
            // left `wipe_hold` standing. See `Active::hold`.
            let mut lc = ended();
            assert!(lc.hold(HoldKind::Wipe));
            assert!(lc.resume());
            assert_eq!(
                lc,
                FightLifecycle::Active {
                    start_ms: 100,
                    hold: Some(HoldKind::Wipe)
                }
            );
            assert_eq!(lc.hold_kind(), Some(HoldKind::Wipe));
        }

        #[test]
        fn resume_is_refused_when_nothing_is_held() {
            let mut lc = FightLifecycle::Idle;
            assert!(!lc.resume());
            assert_eq!(lc, FightLifecycle::Idle);
            assert!(lc.start(100));
            assert!(!lc.resume(), "a running fight has nothing to resume");
            assert_eq!(lc.start_ms(), Some(100));
        }

        #[test]
        fn hold_marks_an_ended_fight() {
            let mut lc = ended();
            assert!(lc.hold(HoldKind::Wipe));
            assert_eq!(lc.hold_kind(), Some(HoldKind::Wipe));
            assert_eq!(lc.end_ms(), Some(900), "the end itself is untouched");
        }

        #[test]
        fn hold_is_refused_twice_and_outside_an_ended_fight() {
            let mut lc = ended();
            assert!(lc.hold(HoldKind::Wipe));
            assert!(!lc.hold(HoldKind::Wipe), "already held");

            let mut idle = FightLifecycle::Idle;
            assert!(!idle.hold(HoldKind::Wipe));
            assert_eq!(idle.hold_kind(), None);

            let mut active = FightLifecycle::Idle;
            assert!(active.start(100));
            assert!(!active.hold(HoldKind::Wipe), "a live fight is not held");
            assert_eq!(active.hold_kind(), None);
        }

        #[test]
        fn release_hold_lifts_the_hold_and_leaves_the_end_alone() {
            let mut lc = ended();
            assert!(lc.hold(HoldKind::Wipe));
            assert!(lc.release_hold());
            assert_eq!(lc.hold_kind(), None);
            assert_eq!(lc.end_ms(), Some(900), "still an ended fight");
            assert_eq!(lc.end_cause(), Some(FightEndCause::BossDeath));
        }

        #[test]
        fn release_hold_is_a_silent_no_op_when_nothing_is_held() {
            let mut lc = FightLifecycle::Idle;
            assert!(!lc.release_hold());
            let mut lc = ended();
            assert!(!lc.release_hold());
            assert_eq!(lc.end_ms(), Some(900));
        }

        #[test]
        fn arm_phase_resume_overwrites_the_armed_id_on_an_ended_fight() {
            let mut lc = ended();
            assert!(lc.arm_phase_resume(Some(207)));
            assert_eq!(lc.phase_resume_boss_id(), Some(207));
            assert_eq!(lc.end_ms(), Some(900));
        }

        #[test]
        fn arm_phase_resume_is_refused_off_an_ended_fight() {
            let mut lc = FightLifecycle::Idle;
            assert!(!lc.arm_phase_resume(Some(103)));
            assert_eq!(lc.phase_resume_boss_id(), None);
            assert!(lc.start(100));
            assert!(!lc.arm_phase_resume(Some(103)));
            assert_eq!(lc.phase_resume_boss_id(), None);
        }

        #[test]
        fn disarm_phase_resume_clears_only_the_arming() {
            let mut lc = ended();
            assert!(lc.disarm_phase_resume());
            assert_eq!(lc.phase_resume_boss_id(), None);
            assert_eq!(lc.end_ms(), Some(900), "the fight is still held");
            assert_eq!(lc.end_cause(), Some(FightEndCause::BossDeath));
            assert!(!lc.disarm_phase_resume(), "nothing left to disarm");
        }

        #[test]
        fn disarm_phase_resume_is_a_silent_no_op_off_an_ended_fight() {
            let mut lc = FightLifecycle::Idle;
            assert!(!lc.disarm_phase_resume());
            assert!(lc.start(100));
            assert!(!lc.disarm_phase_resume());
            assert_eq!(lc.start_ms(), Some(100));
        }

        #[test]
        fn reset_is_legal_from_every_state() {
            for mut lc in [
                FightLifecycle::Idle,
                {
                    let mut a = FightLifecycle::Idle;
                    a.start(100);
                    a
                },
                ended(),
                {
                    let mut h = ended();
                    h.hold(HoldKind::Wipe);
                    h
                },
            ] {
                assert!(lc.reset());
                assert_eq!(lc, FightLifecycle::Idle);
            }
        }
    }
}
