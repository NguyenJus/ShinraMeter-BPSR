//! Reset triggers: manual reset, boss-HP-rollback heuristic, the next
//! fight's first hit (plan §T2.2).
//!
//! `ServerChanged` is deliberately **not** one of these (issue #138): a
//! server change invalidates entity/scene state (`Meter::apply`'s
//! `ServerChanged` arm clears `enemies`/`boss_uid`/`scene_id` directly and
//! freezes the fight clock) but never clears the displayed stats, so it
//! never produces a `ResetReason`. The stats-clearing reset on the far side
//! of a reconnect is `NewFight`, fired by the reconnecting player's first
//! real hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResetReason {
    Manual,
    BossHpRollback,
    /// The previous fight had ended (see `crate::fight`) and its stats were
    /// being held on screen; the first damage of the next fight clears them
    /// (issue #78). Unlike the other reasons this one is *caused* by combat
    /// resuming, so the reset and the new fight's first hit are the same
    /// event. Also the reset that clears a held fight across a
    /// `ServerChanged` reconnect (issue #138): the reconnect itself only
    /// freezes the clock, it does not reset.
    NewFight,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResetConfig {
    pub hp_drop_below_pct: f64,
    pub hp_rollback_at_pct: f64,
    pub cooldown_ms: u64,
}

impl Default for ResetConfig {
    fn default() -> Self {
        Self {
            hp_drop_below_pct: 60.0,
            hp_rollback_at_pct: 90.0,
            cooldown_ms: 2000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnemyState {
    pub curr_hp: Option<u64>,
    pub max_hp: Option<u64>,
    /// High-water mark of every `curr_hp` ever observed for this enemy
    /// (issue #76 / PR #100 review). Stands in for `max_hp` when the appear
    /// packet that carries `AttrMaxHp` was never seen, so a meter started
    /// mid-pull still has a denominator to measure rollbacks against. It is
    /// monotonically non-decreasing and deliberately survives `Meter::reset`
    /// — the peak is a property of the entity, not of the encounter.
    pub peak_hp: Option<u64>,
    pub lowest_pct: Option<f64>,
    pub took_damage: bool,
    /// Whether this enemy has **ever** been damaged by a player, as opposed
    /// to [`Self::took_damage`], which is per-encounter and which
    /// `Meter::reset` clears on every enemy (PR #163 review, finding 2).
    /// Survives `Meter::reset` for the same reason `death_order` does: a
    /// reset is bookkeeping about the *display*, and says nothing about
    /// whether this is an entity the party has been fighting.
    ///
    /// `Meter::recompute_boss` needs exactly that distinction. Its issue
    /// #157 fallback reaches past the enemies damaged in the current fight
    /// — an empty set for the instant after every reset — to the recognized
    /// boss still up, and without this it cannot tell that boss from one
    /// that has only ever stood in AOI range untouched.
    pub ever_damaged: bool,
    /// When this enemy was observed dying during the current fight (issue
    /// #124), as a rank in the encounter's death order: `Some(1)` died
    /// first, `Some(2)` second, `None` has not been seen to die. Set by
    /// either a `DamageEvent::is_dead` naming it as the victim or an
    /// `EnemyHp` syncing its `curr_hp` to 0, whichever lands first.
    ///
    /// Survives `Meter::reset` like the rest of `EnemyState` (PR #144 review,
    /// finding 2): a reset is bookkeeping and says nothing about whether the
    /// corpse got up. What clears it is the respawn itself — an `EnemyHp`
    /// reporting `curr_hp > 0` for an entity that has taken no damage since
    /// that reset. Ranks stay globally monotonic across resets, which is all
    /// `Meter::recompute_boss` needs from them: it only ever compares two
    /// enemies damaged in the *same* encounter.
    ///
    /// Recording an *order* rather than a bare flag serves
    /// `Meter::recompute_boss`: once every damaged enemy in a multi-phase
    /// fight is dead, the most recently killed one is the phase the party
    /// actually just finished, and is what the header should keep naming.
    /// A bare flag would leave that tie to be broken on `max_hp`, which in a
    /// phased fight can name the *first* phase (issue #124's premise is that
    /// an earlier phase carries the larger pool).
    pub death_order: Option<u64>,
    /// Monster template id, from `EnemyHp::monster_id` (issue #9 slice 2).
    /// Survives `Meter::reset` like the rest of `EnemyState` — only a
    /// `ProtocolEvent::ServerChanged` clears the enemy map itself (uids are
    /// re-issued by the new server session, so entity state does not
    /// survive a reconnect the way display state does — issue #138).
    pub monster_id: Option<u32>,
}

impl EnemyState {
    /// Invariant (PR #100 review, finding 1): **every enemy with a known
    /// `curr_hp` has a percentage**, so `check_hp_rollback` — and through it
    /// the wipe/re-pull auto-reset — works for the `curr_hp`-only bosses that
    /// issue #76 made resolvable. Before that invariant, widening
    /// `recompute_boss` to rank on `curr_hp` alone produced a boss that could
    /// *never* trigger a reset: `pct()` returned `None`, `check_hp_rollback`
    /// short-circuited to `false`, and a wipe-and-repull kept accumulating
    /// damage into the previous attempt until the idle timeout fired.
    ///
    /// The denominator is `max_hp` when it is known and non-zero (unchanged
    /// behaviour for that path), otherwise the observed `peak_hp`. Measuring
    /// against the peak means a `curr_hp`-only enemy reads exactly 100% at
    /// every new high, so the bar refilling after a wipe still registers as
    /// a rollback. Healing past the observed peak reads the same way, which
    /// is accepted: it cannot fire on its own, because `check_hp_rollback`
    /// also requires an earlier dip below `hp_drop_below_pct`, and a fresh
    /// pull is safe because `Meter::reset` clears `lowest_pct`.
    pub fn pct(&self) -> Option<f64> {
        let curr = self.curr_hp?;
        let denom = match self.max_hp {
            Some(max) if max > 0 => max,
            _ => self.peak_hp.filter(|peak| *peak > 0)?,
        };
        Some(curr as f64 / denom as f64 * 100.0)
    }

    /// Whether this enemy should be treated as still fighting (issue #124).
    ///
    /// An enemy whose HP was **never observed** counts as alive. That is the
    /// deliberately conservative answer for `Meter::end_fight_on_boss_death`:
    /// mistaking a living boss for a dead one ends the fight early and
    /// under-reports the rest of the encounter (the bug issue #124 is about),
    /// while mistaking a dead boss for a living one only skips the instant
    /// freeze and falls back to the idle timeout, which is always safe.
    ///
    /// That asymmetry only holds for *never observed*, and only because the
    /// second consumer cannot afford it either way (PR #144 review, finding
    /// 2): `Meter::recompute_boss` ranks a living enemy above a dead one, so
    /// a false "alive" there puts the header on a corpse instead of on the
    /// boss actually being fought. A corpse must therefore keep reading dead
    /// for as long as it is one — which is why `death_order` outlives
    /// `Meter::reset` rather than leaving this to a stale `curr_hp`.
    pub fn is_alive(&self) -> bool {
        self.death_order.is_none() && self.curr_hp != Some(0)
    }
}

/// True iff the enemy's HP dropped below `hp_drop_below_pct` at some point
/// during the fight and has since rolled back up to at least
/// `hp_rollback_at_pct` — the signature of a boss-HP-bar reset/wipe rather
/// than genuine burst damage.
pub fn check_hp_rollback(enemy: &EnemyState, cfg: &ResetConfig) -> bool {
    match (enemy.lowest_pct, enemy.pct()) {
        (Some(lowest), Some(current)) => {
            lowest < cfg.hp_drop_below_pct && current >= cfg.hp_rollback_at_pct
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enemy(lowest_pct: Option<f64>, curr_hp: u64, max_hp: u64) -> EnemyState {
        EnemyState {
            curr_hp: Some(curr_hp),
            max_hp: Some(max_hp),
            peak_hp: Some(max_hp),
            lowest_pct,
            took_damage: true,
            ever_damaged: true,
            death_order: None,
            monster_id: None,
        }
    }

    /// An enemy seen only through HP deltas: no `max_hp`, just a running
    /// `curr_hp` and the peak observed so far.
    fn curr_only(lowest_pct: Option<f64>, curr_hp: u64, peak_hp: u64) -> EnemyState {
        EnemyState {
            curr_hp: Some(curr_hp),
            max_hp: None,
            peak_hp: Some(peak_hp),
            lowest_pct,
            took_damage: true,
            ever_damaged: true,
            death_order: None,
            monster_id: None,
        }
    }

    #[test]
    fn pct_computes_from_curr_and_max() {
        let e = enemy(None, 50, 200);
        assert!((e.pct().unwrap() - 25.0).abs() < 0.001);
    }

    #[test]
    fn pct_none_when_no_hp_at_all() {
        let e = EnemyState::default();
        assert_eq!(e.pct(), None);
    }

    #[test]
    fn pct_falls_back_to_the_observed_peak_when_max_hp_is_unknown() {
        let e = curr_only(None, 2_000_000, 8_000_000);
        assert!((e.pct().unwrap() - 25.0).abs() < 0.001);
    }

    #[test]
    fn pct_prefers_max_hp_over_the_observed_peak() {
        let mut e = enemy(None, 50, 200);
        // A peak below `max_hp` (mid-pull join that later learned `max_hp`)
        // must not change the answer.
        e.peak_hp = Some(60);
        assert!((e.pct().unwrap() - 25.0).abs() < 0.001);
    }

    #[test]
    fn pct_treats_a_zero_max_hp_as_unknown() {
        let mut e = enemy(None, 50, 0);
        e.peak_hp = Some(100);
        assert!((e.pct().unwrap() - 50.0).abs() < 0.001);
    }

    #[test]
    fn pct_none_when_neither_max_hp_nor_peak_is_known() {
        let e = EnemyState {
            curr_hp: Some(50),
            ..EnemyState::default()
        };
        assert_eq!(e.pct(), None);
    }

    #[test]
    fn rollback_triggers_for_a_curr_hp_only_enemy_snapping_back_to_its_peak() {
        let cfg = ResetConfig::default();
        // Joined mid-pull at 5M (the peak), burned to 1M (20%), then the bar
        // refilled to the peak on the wipe -> 100% of peak, rollback.
        let e = curr_only(Some(20.0), 5_000_000, 5_000_000);
        assert!(check_hp_rollback(&e, &cfg));
    }

    #[test]
    fn rollback_does_not_trigger_for_a_curr_hp_only_enemy_still_being_burned() {
        let cfg = ResetConfig::default();
        let e = curr_only(Some(20.0), 2_000_000, 5_000_000);
        assert!(!check_hp_rollback(&e, &cfg));
    }

    #[test]
    fn rollback_triggers_when_dropped_below_then_recovered_above() {
        let cfg = ResetConfig::default();
        // lowest 55% (< 60), current 95% (>= 90) -> triggers.
        let e = enemy(Some(55.0), 95, 100);
        assert!(check_hp_rollback(&e, &cfg));
    }

    #[test]
    fn rollback_does_not_trigger_when_never_dropped_below_threshold() {
        let cfg = ResetConfig::default();
        // lowest 70% never dipped below 60 -> no trigger even at 95%.
        let e = enemy(Some(70.0), 95, 100);
        assert!(!check_hp_rollback(&e, &cfg));
    }

    #[test]
    fn rollback_does_not_trigger_when_current_below_recovery_threshold() {
        let cfg = ResetConfig::default();
        // lowest 55% (< 60) but current only 80% (< 90) -> not recovered yet.
        let e = enemy(Some(55.0), 80, 100);
        assert!(!check_hp_rollback(&e, &cfg));
    }

    #[test]
    fn rollback_false_when_lowest_pct_unknown() {
        let cfg = ResetConfig::default();
        let e = enemy(None, 95, 100);
        assert!(!check_hp_rollback(&e, &cfg));
    }
}
