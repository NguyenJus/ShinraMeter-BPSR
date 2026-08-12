//! Reset triggers: manual reset, boss-HP-rollback heuristic, server-change
//! clear (plan §T2.2).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResetReason {
    Manual,
    BossHpRollback,
    ServerChange,
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
    pub lowest_pct: Option<f64>,
    pub took_damage: bool,
}

impl EnemyState {
    pub fn pct(&self) -> Option<f64> {
        match (self.curr_hp, self.max_hp) {
            (Some(curr), Some(max)) if max > 0 => Some(curr as f64 / max as f64 * 100.0),
            _ => None,
        }
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
            lowest_pct,
            took_damage: true,
        }
    }

    #[test]
    fn pct_computes_from_curr_and_max() {
        let e = enemy(None, 50, 200);
        assert!((e.pct().unwrap() - 25.0).abs() < 0.001);
    }

    #[test]
    fn pct_none_when_max_missing() {
        let e = EnemyState::default();
        assert_eq!(e.pct(), None);
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
