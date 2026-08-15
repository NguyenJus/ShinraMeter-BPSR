//! Encounter state machine: routes protocol events into per-player stats and
//! produces the UI-facing `Snapshot` (plan §T2.1/T2.2).

use std::collections::HashMap;

use crate::event::{Class, DamageEvent, EnemyHp, EntityKind, PlayerInfo, ProtocolEvent};
use crate::reset::{EnemyState, ResetConfig, ResetReason, check_hp_rollback};
use crate::stats::{EncounterInfo, PlayerRow, PlayerStats, Snapshot};
use crate::tables;

/// One player-identity cache entry. `seq` is a monotonic touch counter (set
/// on both read and write) used purely to order entries by recency for
/// [`Meter::names_for_save`] — it is never persisted itself, only the
/// resulting order is (see `names_cache::save`'s cap).
#[derive(Clone, Debug, Default)]
struct NameEntry {
    name: Option<String>,
    class: Option<Class>,
    /// Ability score (a.k.a. combat power). Kept in-memory alongside
    /// name/class so it survives a `reset` the same way they do (issue #15).
    /// Deliberately **not** part of the on-disk cache (`names_cache.rs`):
    /// unlike name/class it can drift across sessions (gear changes), so
    /// persisting a stale value risks being more misleading than showing
    /// nothing until a fresh packet arrives.
    ability_score: Option<u32>,
    seq: u64,
}

pub struct Meter {
    players: HashMap<i64, PlayerStats>,
    /// Player identity cache, keyed by uid. Never cleared by `reset` — names
    /// often arrive in packets separate from (and out of order relative to)
    /// damage, so late-named rows must still resolve after a reset. Seeded
    /// at construction time from the on-disk cross-session cache (issue
    /// #12) via [`Meter::with_names_cache`]; live packet data always wins
    /// over a seeded value once it arrives (see `name_upsert`).
    names: HashMap<i64, NameEntry>,
    /// Monotonic counter bumped on every name-cache touch (read or write);
    /// backs the recency order returned by [`Meter::names_for_save`].
    names_seq: u64,
    enemies: HashMap<i64, EnemyState>,
    fight_start_ms: Option<u64>,
    /// Timestamp of the most recent event seen (damage or enemy-hp). Used as
    /// the DPS-window end and as the reference point for the boss-HP-rollback
    /// cooldown gate.
    last_event_ms: u64,
    /// Timestamp of the last reset, if any. `None` means no reset has
    /// happened yet, so the cooldown gate never blocks the first rollback.
    last_reset_ms: Option<u64>,
    boss_uid: Option<i64>,
    /// Current dungeon/instance id (issue #9 slice 2), from the most recent
    /// `ProtocolEvent::Scene`. Survives `Meter::reset` (a manual reset or a
    /// boss-HP rollback both stay in the same dungeon); cleared only on
    /// `ServerChanged`, in `apply` directly rather than in `reset` itself.
    scene_id: Option<u32>,
    reset_cfg: ResetConfig,
}

impl Meter {
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            names: HashMap::new(),
            names_seq: 0,
            enemies: HashMap::new(),
            fight_start_ms: None,
            last_event_ms: 0,
            last_reset_ms: None,
            boss_uid: None,
            scene_id: None,
            reset_cfg: ResetConfig::default(),
        }
    }

    pub fn with_reset_config(cfg: ResetConfig) -> Self {
        Self {
            reset_cfg: cfg,
            ..Self::new()
        }
    }

    /// Seeds the name cache from a previously-persisted uid -> (name, class)
    /// list (issue #12) before any packet has been seen this session, so a
    /// previously-known player resolves instantly instead of showing
    /// `Player {uid}` until their first info/damage packet arrives. Live
    /// packet data received afterwards always takes precedence over a
    /// seeded value (see `name_upsert`).
    ///
    /// `cached` must be in on-disk order, most-recently-used first (see
    /// `names_cache::load`); descending `seq` values are assigned following
    /// that order so the on-disk recency ranking survives the session
    /// boundary instead of being reshuffled into arbitrary iteration order.
    pub fn with_names_cache(cached: crate::names_cache::LoadedNames) -> Self {
        let mut m = Self::new();
        let total = cached.len() as u64;
        for (i, (uid, (name, class))) in cached.into_iter().enumerate() {
            // Index 0 is the most-recently-used on-disk entry, so it gets the
            // highest seq; the last entry gets seq 1.
            let seq = total - i as u64;
            m.names.insert(
                uid,
                NameEntry {
                    name,
                    class,
                    ability_score: None,
                    seq,
                },
            );
        }
        m.names_seq = total;
        m
    }

    /// Reads a cached name/class/ability_score, bumping its recency for
    /// `names_for_save`.
    fn name_lookup(&mut self, uid: i64) -> Option<(Option<String>, Option<Class>, Option<u32>)> {
        self.names_seq += 1;
        let seq = self.names_seq;
        self.names.get_mut(&uid).map(|entry| {
            entry.seq = seq;
            (entry.name.clone(), entry.class, entry.ability_score)
        })
    }

    /// Merges live packet data into the name cache: a `Some` field always
    /// overwrites (live wins over cached/stale data); a `None` field leaves
    /// whatever was already cached untouched. Returns the merged value and
    /// bumps recency.
    fn name_upsert(
        &mut self,
        uid: i64,
        name: Option<String>,
        class: Option<Class>,
        ability_score: Option<u32>,
    ) -> (Option<String>, Option<Class>, Option<u32>) {
        self.names_seq += 1;
        let seq = self.names_seq;
        let entry = self.names.entry(uid).or_default();
        if name.is_some() {
            entry.name = name;
        }
        if class.is_some() {
            entry.class = class;
        }
        if ability_score.is_some() {
            entry.ability_score = ability_score;
        }
        entry.seq = seq;
        (entry.name.clone(), entry.class, entry.ability_score)
    }

    /// Exports the name cache for persistence, ordered most-recently-touched
    /// first so a caller (e.g. `names_cache::save`) that caps the entry
    /// count evicts the least-recently-used entries.
    pub fn names_for_save(&self) -> Vec<(i64, Option<String>, Option<Class>)> {
        let mut entries: Vec<(i64, &NameEntry)> = self.names.iter().map(|(u, e)| (*u, e)).collect();
        entries.sort_by_key(|(uid, e)| std::cmp::Reverse((e.seq, *uid)));
        entries
            .into_iter()
            .map(|(uid, e)| (uid, e.name.clone(), e.class))
            .collect()
    }

    pub fn set_reset_config(&mut self, cfg: ResetConfig) {
        self.reset_cfg = cfg;
    }

    pub fn reset_config(&self) -> &ResetConfig {
        &self.reset_cfg
    }

    /// Routes an event into the encounter state. Returns `Some(reason)` when
    /// applying the event triggered a reset.
    pub fn apply(&mut self, ev: &ProtocolEvent) -> Option<ResetReason> {
        match ev {
            ProtocolEvent::Damage(d) => self.apply_damage(d),
            ProtocolEvent::Player(p) => {
                self.apply_player(p);
                None
            }
            ProtocolEvent::EnemyHp(e) => self.apply_enemy_hp(e),
            ProtocolEvent::Scene { level_map_id } => {
                self.scene_id = Some(*level_map_id);
                None
            }
            ProtocolEvent::ServerChanged { timestamp_ms } => {
                self.reset(ResetReason::ServerChange, *timestamp_ms);
                self.enemies.clear();
                self.boss_uid = None;
                self.scene_id = None;
                Some(ResetReason::ServerChange)
            }
        }
    }

    fn apply_damage(&mut self, d: &DamageEvent) -> Option<ResetReason> {
        // Healing view is a non-goal: heal events never touch damage totals
        // or fight timing.
        if d.is_heal {
            return None;
        }

        if d.target_kind == EntityKind::Monster {
            let enemy = self.enemies.entry(d.target_uid).or_default();
            enemy.took_damage = true;
            self.recompute_boss();
        }

        self.last_event_ms = self.last_event_ms.max(d.timestamp_ms);

        // Only player attackers start the fight clock and produce rows;
        // monster damage is tracked above for boss-selection/reset purposes
        // only. Starting the clock on monster damage would let a boss
        // attacking the tank before players open fire dilute every row's DPS
        // with idle time.
        if d.attacker_kind != EntityKind::Player {
            return None;
        }

        if self.fight_start_ms.is_none() {
            self.fight_start_ms = Some(d.timestamp_ms);
        }

        let cached = self.name_lookup(d.attacker_uid);
        let stats = self
            .players
            .entry(d.attacker_uid)
            .or_insert_with(|| PlayerStats::new(d.attacker_uid));
        if let Some((name, class, ability_score)) = cached {
            if stats.name.is_none() {
                stats.name = name;
            }
            if stats.class.is_none() {
                stats.class = class;
            }
            if stats.ability_score.is_none() {
                stats.ability_score = ability_score;
            }
        }

        stats.hits += 1;
        if !d.is_miss {
            stats.total_damage += d.value;
            if d.crit {
                stats.crit_hits += 1;
                stats.crit_damage += d.value;
            }
            if d.lucky {
                stats.lucky_hits += 1;
                stats.lucky_damage += d.value;
            }
        }

        None
    }

    fn apply_player(&mut self, p: &PlayerInfo) {
        let (name, class, ability_score) =
            self.name_upsert(p.uid, p.name.clone(), p.class, p.ability_score);
        if let Some(stats) = self.players.get_mut(&p.uid) {
            if name.is_some() {
                stats.name = name;
            }
            if class.is_some() {
                stats.class = class;
            }
            if ability_score.is_some() {
                stats.ability_score = ability_score;
            }
        }
    }

    fn apply_enemy_hp(&mut self, e: &EnemyHp) -> Option<ResetReason> {
        // `last_event_ms` is the DPS-window end and must reflect damage
        // only; enemy-HP sync/regen packets arriving after combat stops
        // would otherwise keep extending the denominator and decay DPS
        // toward zero with no combat happening.
        {
            let enemy = self.enemies.entry(e.uid).or_default();
            if e.curr_hp.is_some() {
                enemy.curr_hp = e.curr_hp;
            }
            if e.max_hp.is_some() {
                enemy.max_hp = e.max_hp;
            }
            if e.monster_id.is_some() {
                enemy.monster_id = e.monster_id;
            }
            if let Some(pct) = enemy.pct() {
                enemy.lowest_pct = Some(enemy.lowest_pct.map_or(pct, |lp| lp.min(pct)));
            }
        }

        self.recompute_boss();

        if self.boss_uid == Some(e.uid) {
            let cooldown_ok = match self.last_reset_ms {
                Some(last) => e.timestamp_ms.saturating_sub(last) >= self.reset_cfg.cooldown_ms,
                None => true,
            };
            let should_reset = {
                let enemy = &self.enemies[&e.uid];
                check_hp_rollback(enemy, &self.reset_cfg)
            };
            if should_reset {
                if cooldown_ok {
                    self.reset(ResetReason::BossHpRollback, e.timestamp_ms);
                    return Some(ResetReason::BossHpRollback);
                }
                // The rollback shape was observed but suppressed by the
                // cooldown gate. Latch it so the same rollback can't re-fire
                // the instant the cooldown expires (it's level-triggered on
                // `lowest_pct`, which only clears inside `reset`).
                if let Some(enemy) = self.enemies.get_mut(&e.uid) {
                    enemy.lowest_pct = None;
                }
            }
        }

        None
    }

    /// Boss = the monster uid with the largest known `max_hp` among monsters
    /// that have taken damage in the current fight (plan §T2.2; no boss-name
    /// table, no death/wipe packets).
    fn recompute_boss(&mut self) {
        self.boss_uid = self
            .enemies
            .iter()
            .filter(|(_, e)| e.took_damage)
            .filter_map(|(uid, e)| e.max_hp.map(|hp| (*uid, hp)))
            // Tie-break deterministically on uid: `HashMap` iteration order
            // is unspecified, so breaking ties on `hp` alone let `boss_uid`
            // flip between calls for two enemies sharing the same `max_hp`.
            .max_by_key(|(uid, hp)| (*hp, *uid))
            .map(|(uid, _)| uid);
    }

    /// Clears `players` and per-enemy `lowest_pct`; keeps `names`.
    pub fn reset(&mut self, _reason: ResetReason, now_ms: u64) {
        self.players.clear();
        for enemy in self.enemies.values_mut() {
            enemy.lowest_pct = None;
            enemy.took_damage = false;
        }
        self.fight_start_ms = None;
        self.last_reset_ms = Some(now_ms);
        // No enemy has `took_damage` anymore, so this clears `boss_uid`.
        // Otherwise a stale `boss_uid` from the previous pull would still
        // match an `EnemyHp` packet for the old boss arriving before the
        // next damage event, letting its HP-refill curve fire a second,
        // spurious reset.
        self.recompute_boss();
    }

    pub fn snapshot(&self, now_ms: u64) -> Snapshot {
        let total_damage: i64 = self.players.values().map(|p| p.total_damage).sum();

        let display_duration_ms = match self.fight_start_ms {
            Some(start) => now_ms.saturating_sub(start).max(1),
            None => 0,
        };
        // DPS denominator: last-damage - first-damage, min 1s, so idle time
        // between the caller's `now_ms` and the last hit doesn't dilute DPS.
        let dps_duration_ms = match self.fight_start_ms {
            Some(start) => self.last_event_ms.saturating_sub(start).max(1000),
            None => 1000,
        };

        let mut rows: Vec<PlayerRow> = self
            .players
            .values()
            .map(|p| {
                let dps = p.total_damage as f64 / (dps_duration_ms as f64 / 1000.0);
                let share_pct = if total_damage > 0 {
                    (p.total_damage as f64 / total_damage as f64 * 100.0) as f32
                } else {
                    0.0
                };
                PlayerRow {
                    uid: p.uid,
                    name: p
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Player {}", p.uid)),
                    class: p.class,
                    ability_score: p.ability_score,
                    damage: p.total_damage,
                    dps,
                    share_pct,
                    crit_pct: p.crit_pct(),
                    lucky_pct: p.lucky_pct(),
                    hits: p.hits,
                }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.damage));

        let total_dps = total_damage as f64 / (dps_duration_ms as f64 / 1000.0);

        let boss_monster_id = self
            .boss_uid
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.monster_id);
        let encounter = EncounterInfo {
            boss_monster_id,
            boss_name: boss_monster_id.and_then(tables::monster_name),
            scene_id: self.scene_id,
            scene_name: self.scene_id.and_then(tables::scene_name),
        };

        Snapshot {
            duration_ms: display_duration_ms,
            total_damage,
            total_dps,
            rows,
            encounter,
        }
    }

    pub fn is_active(&self) -> bool {
        self.fight_start_ms.is_some()
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dmg(attacker_uid: i64, value: i64, ts: u64) -> ProtocolEvent {
        ProtocolEvent::Damage(DamageEvent {
            attacker_uid,
            attacker_kind: EntityKind::Player,
            value,
            timestamp_ms: ts,
            ..Default::default()
        })
    }

    #[test]
    fn two_attackers_ordering_and_share() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 700, 1000));
        m.apply(&dmg(2, 300, 1000));
        let snap = m.snapshot(2000);
        assert_eq!(snap.total_damage, 1000);
        assert_eq!(snap.rows.len(), 2);
        assert_eq!(snap.rows[0].uid, 1);
        assert_eq!(snap.rows[1].uid, 2);
        assert!((snap.rows[0].share_pct - 70.0).abs() < 0.01);
        assert!((snap.rows[1].share_pct - 30.0).abs() < 0.01);
    }

    #[test]
    fn heal_excluded_from_damage() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            value: 500,
            is_heal: true,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.total_damage, 0);
        assert!(snap.rows.is_empty());
        assert!(!m.is_active());
    }

    #[test]
    fn miss_counts_as_hit_with_zero_damage() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            value: 999,
            is_miss: true,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.total_damage, 0);
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].hits, 1);
        assert_eq!(snap.rows[0].damage, 0);
    }

    #[test]
    fn player_info_after_damage_renames_row() {
        let mut m = Meter::new();
        m.apply(&dmg(5, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 5,
            name: Some("Foo".to_string()),
            class: Some(Class::Stormblade),
            ability_score: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].name, "Foo");
        assert_eq!(snap.rows[0].class, Some(Class::Stormblade));
    }

    #[test]
    fn player_info_ability_score_reaches_row() {
        let mut m = Meter::new();
        m.apply(&dmg(7, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 7,
            name: None,
            class: None,
            ability_score: Some(45_000),
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].ability_score, Some(45_000));
    }

    #[test]
    fn ability_score_survives_reset_like_name_and_class() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 3,
            name: Some("Foo".to_string()),
            class: None,
            ability_score: Some(1000),
        }));
        m.apply(&dmg(3, 100, 0));
        m.reset(ResetReason::Manual, 1000);
        m.apply(&dmg(3, 50, 2000));
        let snap = m.snapshot(3000);
        assert_eq!(snap.rows[0].ability_score, Some(1000));
    }

    #[test]
    fn unnamed_player_row_falls_back_to_player_uid() {
        let mut m = Meter::new();
        m.apply(&dmg(42, 100, 1000));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].name, "Player 42");
    }

    #[test]
    fn dps_uses_last_damage_minus_first_damage_window() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 5000, 0));
        m.apply(&dmg(1, 5000, 10_000));
        // now_ms is far beyond the last hit; DPS must not be diluted by idle time.
        let snap = m.snapshot(60_000);
        assert!((snap.rows[0].dps - 1000.0).abs() < 0.01);
    }

    #[test]
    fn header_total_dps_matches_row_dps_on_first_tick() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 5000, 1_000_000));
        // now_ms is called 1ms after the fight-start timestamp: display
        // duration would be 1ms, but the DPS window (last_event - start,
        // min 1000ms) is 1000ms. The header's total_dps must use the same
        // window as the row, not the display duration.
        let snap = m.snapshot(1_000_001);
        assert_eq!(snap.rows.len(), 1);
        assert!((snap.total_dps - snap.rows[0].dps).abs() < 0.01);
        assert!((snap.total_dps - 5000.0).abs() < 0.01);
    }

    #[test]
    fn fight_clock_does_not_start_on_monster_damage() {
        let mut m = Meter::new();
        // Boss hits a player at t=0; the clock must not start yet.
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 99,
            attacker_kind: EntityKind::Monster,
            target_uid: 1,
            target_kind: EntityKind::Player,
            value: 500,
            timestamp_ms: 0,
            ..Default::default()
        }));
        assert!(!m.is_active());

        // Players only open 60s later.
        m.apply(&dmg(1, 1000, 60_000));
        let snap = m.snapshot(61_000);
        // DPS window must be anchored to the first *player* damage (60_000),
        // not the earlier monster damage (0), or the 60s of idle time halves
        // (here, 60x-diminishes) every row's DPS.
        assert!((snap.rows[0].dps - 1000.0).abs() < 0.01);
    }

    #[test]
    fn enemy_hp_packet_does_not_extend_dps_window() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 1000, 0));
        m.apply(&dmg(1, 1000, 1000));

        // A boss-HP sync/regen tick arrives long after combat stopped.
        m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
            uid: 10,
            curr_hp: Some(100),
            max_hp: Some(100),
            timestamp_ms: 60_000,
            ..Default::default()
        }));

        let snap = m.snapshot(61_000);
        // DPS window is last-damage(1000) - first-damage(0) = 1s, not 60s.
        assert!((snap.rows[0].dps - 2000.0).abs() < 0.01);
    }

    #[test]
    fn monster_attacker_produces_no_row() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 99,
            attacker_kind: EntityKind::Monster,
            target_uid: 1,
            target_kind: EntityKind::Player,
            value: 200,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        assert!(snap.rows.is_empty());
    }

    mod names_cache {
        use super::*;

        #[test]
        fn cached_name_resolves_before_any_packet_arrives_this_session() {
            let cache = vec![(5, (Some("Cached".to_string()), Some(Class::Marksman)))];
            let mut m = Meter::with_names_cache(cache);

            // No PlayerInfo event this session — only damage.
            m.apply(&dmg(5, 100, 1000));

            let snap = m.snapshot(2000);
            assert_eq!(snap.rows[0].name, "Cached");
            assert_eq!(snap.rows[0].class, Some(Class::Marksman));
        }

        #[test]
        fn live_player_info_overrides_cached_name() {
            let cache = vec![(5, (Some("Stale".to_string()), Some(Class::Marksman)))];
            let mut m = Meter::with_names_cache(cache);

            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 5,
                name: Some("Fresh".to_string()),
                class: Some(Class::FrostMage),
                ability_score: None,
            }));
            m.apply(&dmg(5, 100, 1000));

            let snap = m.snapshot(2000);
            assert_eq!(snap.rows[0].name, "Fresh");
            assert_eq!(snap.rows[0].class, Some(Class::FrostMage));
        }

        #[test]
        fn live_partial_update_keeps_cached_field_it_did_not_supply() {
            let cache = vec![(5, (Some("Cached".to_string()), Some(Class::Marksman)))];
            let mut m = Meter::with_names_cache(cache);

            // Live packet only carries a name this time, no class.
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 5,
                name: Some("Renamed".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&dmg(5, 100, 1000));

            let snap = m.snapshot(2000);
            assert_eq!(snap.rows[0].name, "Renamed");
            assert_eq!(snap.rows[0].class, Some(Class::Marksman));
        }

        #[test]
        fn names_for_save_round_trips_through_with_names_cache() {
            let cache = vec![
                (1, (Some("Alice".to_string()), Some(Class::Marksman))),
                (2, (Some("Bob".to_string()), None)),
            ];
            let m = Meter::with_names_cache(cache);

            let saved = m.names_for_save();
            assert_eq!(saved.len(), 2);
            assert!(saved.contains(&(1, Some("Alice".to_string()), Some(Class::Marksman))));
            assert!(saved.contains(&(2, Some("Bob".to_string()), None)));
        }

        #[test]
        fn with_names_cache_assigns_seq_following_on_disk_order() {
            // `cached` is in on-disk order, most-recently-used first (as
            // `names_cache::load` returns it). The resulting recency order
            // (via `names_for_save`) must follow that order exactly, not an
            // arbitrary HashMap-derived order.
            let cache = vec![
                (30, (Some("Thirty".to_string()), None)),
                (10, (Some("Ten".to_string()), None)),
                (20, (Some("Twenty".to_string()), None)),
            ];
            let m = Meter::with_names_cache(cache);

            let saved = m.names_for_save();
            let order: Vec<i64> = saved.iter().map(|(uid, _, _)| *uid).collect();
            assert_eq!(order, vec![30, 10, 20]);
        }

        #[test]
        fn load_save_round_trip_preserves_relative_recency_order() {
            let path = bpsr_test_support::scratch_path("load-save-order");

            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("A".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 2,
                name: Some("B".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 3,
                name: Some("C".to_string()),
                class: None,
                ability_score: None,
            }));
            // Re-touch uid 1 so it becomes the most recently used, ahead of
            // 3 and 2 (in that order).
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("A".to_string()),
                class: None,
                ability_score: None,
            }));

            let before = m.names_for_save();
            let order_before: Vec<i64> = before.iter().map(|(uid, _, _)| *uid).collect();
            assert_eq!(order_before, vec![1, 3, 2]);

            crate::names_cache::save(&path, &before);
            let loaded = crate::names_cache::load(&path);
            let m2 = Meter::with_names_cache(loaded);
            let after = m2.names_for_save();
            let order_after: Vec<i64> = after.iter().map(|(uid, _, _)| *uid).collect();

            assert_eq!(order_before, order_after);

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn names_for_save_orders_most_recently_touched_first() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("First".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 2,
                name: Some("Second".to_string()),
                class: None,
                ability_score: None,
            }));

            let saved = m.names_for_save();
            assert_eq!(saved[0].0, 2);
            assert_eq!(saved[1].0, 1);
        }

        #[test]
        fn server_change_reset_preserves_names_for_save() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 1000 });

            let saved = m.names_for_save();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].0, 1);
        }
    }

    mod reset {
        use super::*;

        fn hp(uid: i64, curr: u64, max: u64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        fn boss_hit(uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 1,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        #[test]
        fn rollback_100_to_55_to_95_triggers_once() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.apply(&hp(10, 100, 100, 0)), None);
            assert_eq!(m.apply(&hp(10, 55, 100, 100)), None);
            let r = m.apply(&hp(10, 95, 100, 200));
            assert_eq!(r, Some(ResetReason::BossHpRollback));
        }

        #[test]
        fn rollback_100_to_70_to_95_never_triggers() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            m.apply(&hp(10, 70, 100, 100));
            let r = m.apply(&hp(10, 95, 100, 200));
            assert_eq!(r, None);
        }

        #[test]
        fn two_rollbacks_within_cooldown_trigger_once() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            m.apply(&hp(10, 55, 100, 100));
            let first = m.apply(&hp(10, 95, 100, 200));
            assert_eq!(first, Some(ResetReason::BossHpRollback));

            // 500ms later (< 2000ms cooldown): a second drop/recover must not fire.
            m.apply(&hp(10, 55, 100, 300));
            let second = m.apply(&hp(10, 95, 100, 700));
            assert_eq!(second, None);
        }

        #[test]
        fn cooldown_suppressed_rollback_does_not_refire_after_cooldown_expires() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            m.apply(&hp(10, 55, 100, 100));
            let first = m.apply(&hp(10, 95, 100, 200));
            assert_eq!(first, Some(ResetReason::BossHpRollback));

            // Within cooldown (last_reset_ms=200, cooldown=2000): the same
            // drop/recover shape is observed but suppressed.
            m.apply(&hp(10, 55, 100, 300));
            let suppressed = m.apply(&hp(10, 95, 100, 700));
            assert_eq!(suppressed, None);

            // Cooldown has now expired (2300 - 200 = 2100ms >= 2000ms). The
            // suppressed rollback must not re-fire just because the cooldown
            // gate opened again.
            let after_cooldown = m.apply(&hp(10, 96, 100, 2300));
            assert_eq!(after_cooldown, None);
        }

        #[test]
        fn recompute_boss_tie_break_is_deterministic_on_uid() {
            // Two enemies tied on max_hp; insertion order differs between the
            // two Meters. The tie-break must not depend on HashMap iteration
            // order.
            let mut m1 = Meter::new();
            m1.apply(&boss_hit(5, 0));
            m1.apply(&hp(5, 100, 100, 0));
            m1.apply(&boss_hit(10, 0));
            m1.apply(&hp(10, 100, 100, 0));
            m1.apply(&boss_hit(7, 0));
            m1.apply(&hp(7, 100, 100, 0));

            let mut m2 = Meter::new();
            m2.apply(&boss_hit(7, 0));
            m2.apply(&hp(7, 100, 100, 0));
            m2.apply(&boss_hit(10, 0));
            m2.apply(&hp(10, 100, 100, 0));
            m2.apply(&boss_hit(5, 0));
            m2.apply(&hp(5, 100, 100, 0));

            assert_eq!(m1.boss_uid, Some(10));
            assert_eq!(m2.boss_uid, Some(10));
        }

        #[test]
        fn server_change_cooldown_uses_change_time_not_stale_last_event_ms() {
            let mut m = Meter::new();
            // Old fight, long since idle.
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));

            // Server change detected 5 minutes later.
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 300_000,
            });

            // New zone: boss picked up again almost immediately.
            m.apply(&boss_hit(10, 300_050));
            m.apply(&hp(10, 55, 100, 300_060));
            let r = m.apply(&hp(10, 96, 100, 300_100));

            // This looks like a rollback shape, but the cooldown (anchored to
            // the server-change moment, not the stale last-event time) hasn't
            // elapsed yet -> must be suppressed.
            assert_eq!(r, None);
        }

        #[test]
        fn reset_clears_boss_uid_so_stale_hp_packet_cannot_refire() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            assert_eq!(m.boss_uid, Some(10));

            m.reset(ResetReason::Manual, 1000);
            assert_eq!(m.boss_uid, None);

            // An HP packet for the old boss uid, arriving before any new
            // damage picks a new boss, must not be able to drive a reset off
            // the stale boss_uid.
            let r = m.apply(&hp(10, 55, 100, 1100));
            assert_eq!(r, None);
        }

        #[test]
        fn reset_clears_took_damage_on_all_enemies() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            assert!(m.enemies[&10].took_damage);
            m.reset(ResetReason::Manual, 1000);
            assert!(!m.enemies[&10].took_damage);
        }

        #[test]
        fn server_changed_clears_players_and_enemies() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 0));
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            let r = m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 1000 });
            assert_eq!(r, Some(ResetReason::ServerChange));
            let snap = m.snapshot(1000);
            assert!(snap.rows.is_empty());
            assert!(m.enemies.is_empty());
            assert!(m.boss_uid.is_none());
        }

        #[test]
        fn manual_reset_keeps_name_cache_for_late_damage() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
            }));
            m.apply(&dmg(1, 100, 0));
            m.reset(ResetReason::Manual, 1000);
            assert!(m.players.is_empty());
            m.apply(&dmg(1, 50, 2000));
            let snap = m.snapshot(3000);
            assert_eq!(snap.rows[0].name, "Foo");
        }
    }

    mod encounter_info {
        use super::*;

        fn boss_hit(uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 1,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        fn hp(uid: i64, curr: u64, max: u64, monster_id: Option<u32>, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id,
                timestamp_ms: ts,
            })
        }

        #[test]
        fn boss_name_resolves_for_a_known_monster_id() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert_eq!(snap.encounter.boss_name, Some("Rathalos"));
        }

        #[test]
        fn unnamed_monster_id_yields_id_without_a_name() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(999_999), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(999_999));
            assert_eq!(snap.encounter.boss_name, None);
        }

        #[test]
        fn no_boss_yet_yields_no_boss_monster_id_or_name() {
            let m = Meter::new();
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, None);
            assert_eq!(snap.encounter.boss_name, None);
        }

        #[test]
        fn scene_survives_a_manual_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.reset(ResetReason::Manual, 1000);
            let snap = m.snapshot(2000);
            assert_eq!(snap.encounter.scene_id, Some(1001));
            assert_eq!(snap.encounter.scene_name, Some("Tina's Mindrealm"));
        }

        #[test]
        fn scene_survives_a_boss_hp_rollback_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.reset(ResetReason::BossHpRollback, 1000);
            let snap = m.snapshot(2000);
            assert_eq!(snap.encounter.scene_id, Some(1001));
        }

        #[test]
        fn scene_clears_on_server_change() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 1000 });
            let snap = m.snapshot(2000);
            assert_eq!(snap.encounter.scene_id, None);
            assert_eq!(snap.encounter.scene_name, None);
        }

        #[test]
        fn unknown_scene_id_yields_id_without_a_name() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: 999_999,
            });
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_id, Some(999_999));
            assert_eq!(snap.encounter.scene_name, None);
        }
    }
}
