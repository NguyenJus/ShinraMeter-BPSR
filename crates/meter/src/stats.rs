//! Per-player stats and the UI-facing snapshot read model (plan §T2.1).

use crate::event::Class;

#[derive(Debug, Clone)]
pub struct PlayerStats {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    pub total_damage: i64,
    pub hits: u64,
    pub crit_hits: u64,
    pub crit_damage: i64,
    pub lucky_hits: u64,
    pub lucky_damage: i64,
}

impl PlayerStats {
    pub fn new(uid: i64) -> Self {
        Self {
            uid,
            name: None,
            class: None,
            total_damage: 0,
            hits: 0,
            crit_hits: 0,
            crit_damage: 0,
            lucky_hits: 0,
            lucky_damage: 0,
        }
    }

    pub fn crit_pct(&self) -> f32 {
        if self.hits == 0 {
            0.0
        } else {
            self.crit_hits as f32 / self.hits as f32 * 100.0
        }
    }

    pub fn lucky_pct(&self) -> f32 {
        if self.hits == 0 {
            0.0
        } else {
            self.lucky_hits as f32 / self.hits as f32 * 100.0
        }
    }
}

/// The UI's read model for one row of the meter table.
#[derive(Debug, Clone)]
pub struct PlayerRow {
    pub uid: i64,
    pub name: String,
    pub class: Option<Class>,
    pub damage: i64,
    pub dps: f64,
    pub share_pct: f32,
    pub crit_pct: f32,
    pub lucky_pct: f32,
    pub hits: u64,
}

/// Cheap, immutable snapshot of the current encounter, sorted by damage
/// descending.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub duration_ms: u64,
    pub total_damage: i64,
    pub rows: Vec<PlayerRow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crit_pct_zero_hits_is_zero() {
        let s = PlayerStats::new(1);
        assert_eq!(s.crit_pct(), 0.0);
    }

    #[test]
    fn crit_pct_computes_percentage() {
        let mut s = PlayerStats::new(1);
        s.hits = 10;
        s.crit_hits = 3;
        assert!((s.crit_pct() - 30.0).abs() < 0.001);
    }

    #[test]
    fn lucky_pct_zero_hits_is_zero() {
        let s = PlayerStats::new(1);
        assert_eq!(s.lucky_pct(), 0.0);
    }

    #[test]
    fn lucky_pct_computes_percentage() {
        let mut s = PlayerStats::new(1);
        s.hits = 4;
        s.lucky_hits = 1;
        assert!((s.lucky_pct() - 25.0).abs() < 0.001);
    }
}
