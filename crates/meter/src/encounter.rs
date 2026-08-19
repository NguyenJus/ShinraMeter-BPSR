//! Encounter state machine: routes protocol events into per-player stats and
//! produces the UI-facing `Snapshot` (plan §T2.1/T2.2).

use std::collections::HashMap;

use crate::event::{Class, DamageEvent, EnemyHp, EntityKind, PlayerInfo, ProtocolEvent};
use crate::fight::{FightConfig, FightState};
use crate::reset::{EnemyState, ResetConfig, ResetReason, check_hp_rollback};
use crate::stats::{EncounterInfo, PlayerRow, PlayerStats, Snapshot};
use crate::tables;

/// Debounce window for `DamageEvent::is_dead`: a death for the same uid
/// counted within this many milliseconds of the last one is treated as a
/// duplicate (this repo's TCP reassembly tolerates retransmits, so a delta
/// packet can legitimately arrive twice) rather than a second, real death.
/// 2000ms, matching resonance-logs' reference value for the same signal
/// (issue #49).
const DEATH_DEBOUNCE_MS: u64 = 2000;

/// Defensive ceiling on preloaded roster rows per scene (issue #12/#145
/// finding 3). The preload path in `apply_player` is gated on
/// `in_dungeon_scene`, i.e. `tables::is_dungeon_scene` — generated data
/// (see `tables.rs`) this code has no way to validate. If a scene is ever
/// misclassified as a dungeon, this stops `players` from growing
/// unboundedly instead of relying solely on that classification being
/// correct. Set comfortably above the largest real raid this meter
/// supports (20 players — see
/// `preloading_a_full_20_player_raid_snapshots_cleanly`), so it never
/// affects a real dungeon or raid.
const MAX_PRELOADED_PLAYERS: u32 = 64;

/// One player-identity cache entry. `seq` is a monotonic touch counter (set
/// on both read and write) used purely to order entries by recency for
/// [`Meter::names_for_save`] — it is never persisted itself, only the
/// resulting order is (see `names_cache::save`'s cap).
#[derive(Clone, Debug, Default)]
struct NameEntry {
    name: Option<String>,
    class: Option<Class>,
    /// Ability score (a.k.a. combat power) and season strength. Kept
    /// in-memory alongside name/class so each survives a `reset` the same
    /// way they do (issue #15). Deliberately **not** part of the on-disk
    /// cache (`names_cache.rs`): unlike name/class these can drift across
    /// sessions (gear changes, season progression), so persisting a stale
    /// value risks being more misleading than showing nothing until a fresh
    /// packet arrives.
    ability_score: Option<u32>,
    season_strength: Option<u32>,
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    imagines: Option<[Option<i32>; 2]>,
    seq: u64,
}

/// The identity/stat fields threaded through `name_lookup`/`name_upsert`.
/// Grouping these as a named struct, rather than a positional tuple, avoids
/// transposing same-typed fields (two of these four are `Option<u32>`) at
/// a call site.
#[derive(Clone, Debug, Default)]
struct CachedAttrs {
    name: Option<String>,
    class: Option<Class>,
    ability_score: Option<u32>,
    season_strength: Option<u32>,
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    imagines: Option<[Option<i32>; 2]>,
}

/// Copies the five cacheable identity fields (name/class/ability_score/
/// season_strength/imagines) from `merged` onto `stats`, one at a time,
/// skipping any field `merged` has no opinion on (issue #145 finding 6: this
/// list used to be duplicated between `apply_player`'s existing-row and
/// preload branches, so adding a sixth cached field meant editing both). A
/// freshly `PlayerStats::new` row starts with every one of these fields at
/// `None`, so the per-field guard is a no-op there — this same guarded copy
/// is exactly equivalent to an unconditional one for a fresh row, which is
/// what lets both branches share it.
fn apply_cached_attrs(stats: &mut PlayerStats, merged: CachedAttrs) {
    if merged.name.is_some() {
        stats.name = merged.name;
    }
    if merged.class.is_some() {
        stats.class = merged.class;
    }
    if merged.ability_score.is_some() {
        stats.ability_score = merged.ability_score;
    }
    if merged.season_strength.is_some() {
        stats.season_strength = merged.season_strength;
    }
    if merged.imagines.is_some() {
        stats.imagines = merged.imagines;
    }
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
    /// Learned final boss per dungeon scene (issue #125): maps a dungeon
    /// scene id (`tables::is_dungeon_scene`) to the monster id of the last
    /// genuine boss (`tables::is_boss_monster`) engaged there. No upstream
    /// data table maps a scene to its final boss, so this is *learned* by
    /// observation instead — the last real boss fought in a dungeon is
    /// assumed to be its final boss, which converges correctly because a
    /// dungeon's own boss order runs earlier bosses before its last one.
    /// Deliberately session-lifetime, **not** cleared by `reset`: an
    /// encounter reset (or a boss-HP rollback) happens mid-dungeon, and the
    /// whole point is for the remembered name to survive into the *next*
    /// pull and the *next* run of the same dungeon, not just the current
    /// one. Not persisted to disk — see the issue #125 design note on
    /// `recompute_boss`.
    scene_bosses: HashMap<u32, u32>,
    /// When the current fight ended, if it has (issue #78). `Some(t)` puts
    /// the meter in [`FightState::Ended`]: the snapshot is rendered as of
    /// `t` rather than the caller's `now_ms`, so rows, totals and the
    /// elapsed timer all hold still until the next fight (or a manual reset
    /// / server change) clears them. Latched by an explicit end signal (a
    /// boss death) or by [`Meter::tick`] once the idle timeout has elapsed;
    /// cleared by `reset`.
    fight_end_ms: Option<u64>,
    reset_cfg: ResetConfig,
    fight_cfg: FightConfig,
    /// Count of `PlayerStats` rows created by the dungeon-gated preload path
    /// (issue #12) in the *current* scene. `apply_player`/`name_upsert` have
    /// zero per-player `log::` calls, deliberately, to avoid a per-raid-member
    /// flood; this counter is what lets `prune_stale_preloads` log a single
    /// sparse summary line per scene transition instead, answering whether
    /// AOI actually delivers every party member's identity in a large raid.
    /// Reset to zero on every real scene transition.
    preload_count: u32,
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
            scene_bosses: HashMap::new(),
            fight_end_ms: None,
            reset_cfg: ResetConfig::default(),
            fight_cfg: FightConfig::default(),
            preload_count: 0,
        }
    }

    pub fn with_reset_config(cfg: ResetConfig) -> Self {
        Self {
            reset_cfg: cfg,
            ..Self::new()
        }
    }

    pub fn with_fight_config(cfg: FightConfig) -> Self {
        Self {
            fight_cfg: cfg,
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
                    season_strength: None,
                    imagines: None,
                    seq,
                },
            );
        }
        m.names_seq = total;
        m
    }

    /// Reads a cached name/class/ability_score, bumping its recency for
    /// `names_for_save`.
    fn name_lookup(&mut self, uid: i64) -> Option<CachedAttrs> {
        self.names_seq += 1;
        let seq = self.names_seq;
        self.names.get_mut(&uid).map(|entry| {
            entry.seq = seq;
            CachedAttrs {
                name: entry.name.clone(),
                class: entry.class,
                ability_score: entry.ability_score,
                season_strength: entry.season_strength,
                imagines: entry.imagines,
            }
        })
    }

    /// Merges live packet data into the name cache: a `Some` field always
    /// overwrites (live wins over cached/stale data); a `None` field leaves
    /// whatever was already cached untouched. Returns the merged value and
    /// bumps recency.
    fn name_upsert(&mut self, uid: i64, incoming: CachedAttrs) -> CachedAttrs {
        self.names_seq += 1;
        let seq = self.names_seq;
        let entry = self.names.entry(uid).or_default();
        if incoming.name.is_some() {
            entry.name = incoming.name;
        }
        if incoming.class.is_some() {
            entry.class = incoming.class;
        }
        if incoming.ability_score.is_some() {
            entry.ability_score = incoming.ability_score;
        }
        if incoming.season_strength.is_some() {
            entry.season_strength = incoming.season_strength;
        }
        if incoming.imagines.is_some() {
            entry.imagines = incoming.imagines;
        }
        entry.seq = seq;
        CachedAttrs {
            name: entry.name.clone(),
            class: entry.class,
            ability_score: entry.ability_score,
            season_strength: entry.season_strength,
            imagines: entry.imagines,
        }
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

    pub fn set_fight_config(&mut self, cfg: FightConfig) {
        self.fight_cfg = cfg;
    }

    pub fn fight_config(&self) -> &FightConfig {
        &self.fight_cfg
    }

    /// The moment the current fight ended, or `None` while it is still
    /// running (or while there is no fight at all).
    ///
    /// Two ways a fight ends (issue #78):
    /// * an explicit end signal already latched into `fight_end_ms` — today
    ///   only a recognized boss dying, see `FightConfig::end_on_boss_death`;
    /// * the idle timeout: no damage event for `idle_timeout_ms`. That one is
    ///   derived from `last_event_ms` on every call rather than requiring a
    ///   `tick`, so a caller that only ever calls `snapshot` still gets the
    ///   hold — `tick` merely pins it.
    ///
    /// The end time is the last damage event, not "now": the fight really
    /// ended when the hitting stopped, and using it keeps the frozen elapsed
    /// timer consistent with the DPS window (which is also last-damage
    /// anchored).
    fn fight_ended_at(&self, now_ms: u64) -> Option<u64> {
        self.fight_start_ms?;
        if let Some(end_ms) = self.fight_end_ms {
            return Some(end_ms);
        }
        let idle = self.fight_cfg.idle_timeout_ms;
        if idle > 0 && now_ms.saturating_sub(self.last_event_ms) >= idle {
            Some(self.last_event_ms)
        } else {
            None
        }
    }

    /// Where the meter is in the fight lifecycle as of `now_ms`.
    pub fn fight_state(&self, now_ms: u64) -> FightState {
        match self.fight_start_ms {
            None => FightState::Idle,
            Some(_) if self.fight_ended_at(now_ms).is_some() => FightState::Ended,
            Some(_) => FightState::Active,
        }
    }

    /// Whether the last fight's stats are being held on screen.
    pub fn is_fight_ended(&self, now_ms: u64) -> bool {
        self.fight_state(now_ms) == FightState::Ended
    }

    /// Advances wall-clock-driven fight state and returns the resulting
    /// state. Call this once per UI tick before `snapshot`; it latches an
    /// idle-detected end so the held snapshot can never drift afterwards
    /// (e.g. if the idle timeout is reconfigured mid-hold).
    ///
    /// Deliberately does **not** clear anything: leaving [`FightState::Ended`]
    /// is driven by combat activity (or an explicit reset), never by time
    /// passing — idling in town must not wipe the numbers the user is trying
    /// to screenshot.
    pub fn tick(&mut self, now_ms: u64) -> FightState {
        if let Some(end_ms) = self.fight_ended_at(now_ms) {
            self.fight_end_ms = Some(end_ms);
            FightState::Ended
        } else if self.fight_start_ms.is_some() {
            FightState::Active
        } else {
            FightState::Idle
        }
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
                // Sparse, transition-only diagnostic (issue #69): a scene
                // sync packet can repeat while the player stays in the same
                // instance, so log only when the resolved id actually
                // changes — never per packet, which would just be a smaller
                // version of the #87 flood this exists to avoid.
                if let Some(msg) = scene_transition_log(self.scene_id, Some(*level_map_id)) {
                    log::info!("{msg}");
                }
                // issue #12: any real scene change (entering a dungeon,
                // leaving one, or hopping dungeon -> different dungeon) can
                // leave behind preloaded roster rows nobody ever damaged —
                // drop those now, before the new scene id lands, so a stale
                // party member from the last run doesn't linger into this
                // one. Rows with real activity (damage, a miss, or a death)
                // are untouched; the existing reset machinery still governs
                // those.
                if self.scene_id != Some(*level_map_id) {
                    self.prune_stale_preloads();
                }
                self.scene_id = Some(*level_map_id);
                None
            }
            ProtocolEvent::ServerChanged { timestamp_ms } => {
                // issue #12/#145 finding 4: a server change is as real a
                // scene change as any (the old scene's preloads can't
                // possibly still be in AOI range afterward), so mirror the
                // Scene arm above and prune stale preloads — and log the
                // same summary line — before `reset` clears `players` and
                // `scene_id` is cleared below.
                self.prune_stale_preloads();
                self.reset(ResetReason::ServerChange, *timestamp_ms);
                self.enemies.clear();
                self.boss_uid = None;
                if let Some(msg) = scene_transition_log(self.scene_id, None) {
                    log::info!("{msg}");
                }
                self.scene_id = None;
                Some(ResetReason::ServerChange)
            }
        }
    }

    fn apply_damage(&mut self, d: &DamageEvent) -> Option<ResetReason> {
        // issue #78: pin the end *before* this event touches the encounter's
        // clocks — a monster's swing at a player extends `last_event_ms`
        // without ever producing a row, which would otherwise drag an
        // already-ended fight back into `Active`.
        if let Some(end_ms) = self.fight_ended_at(d.timestamp_ms) {
            self.fight_end_ms = Some(end_ms);
        }

        // Real combat activity — a player landing a hit — is the *only*
        // thing that ends the hold, and it does so through the existing
        // reset machinery, so this event lands in a clean encounter. Gated
        // on the same condition that starts the fight clock below: a monster
        // swinging at a player in town, or a heal, must not wipe the numbers
        // the user is looking at.
        let mut reason = None;
        if self.fight_end_ms.is_some() && d.attacker_kind == EntityKind::Player && !d.is_heal {
            self.reset(ResetReason::NewFight, d.timestamp_ms);
            reason = Some(ResetReason::NewFight);
        }

        // Still held (the reset above clears `fight_end_ms`, so this can only
        // be the "combat the user isn't part of" case): the displayed fight
        // is frozen, so nothing this event carries — not damage, not deaths,
        // not the event clock — may touch it.
        if self.fight_end_ms.is_some() {
            return None;
        }

        // `d.is_dead` flags that `target_uid` (the victim, not the
        // attacker) died from this hit — count it against the target
        // regardless of who or what dealt the blow (issue #49), and
        // regardless of whether the killing packet is heal-typed (e.g. a
        // negative/lethal heal). This must run before the `is_heal` early
        // return below so heal-typed death packets still record deaths.
        if d.is_dead && d.target_kind == EntityKind::Player {
            self.record_death(d.target_uid, d.timestamp_ms);
        }

        // Healing view is a non-goal: heal events never touch damage totals
        // or fight timing.
        if d.is_heal {
            return reason;
        }

        if d.target_kind == EntityKind::Monster {
            let enemy = self.enemies.entry(d.target_uid).or_default();
            enemy.took_damage = true;
            self.recompute_boss();
            // issue #78: a recognized boss dying ends the fight now, rather
            // than after the idle timeout, so the meter freezes on the kill
            // instead of on a straggler's last tick of DoT damage.
            if self.fight_cfg.end_on_boss_death && d.is_dead && self.boss_uid == Some(d.target_uid)
            {
                self.end_fight_on_boss_death(d.target_uid, d.timestamp_ms);
            }
        }

        self.last_event_ms = self.last_event_ms.max(d.timestamp_ms);

        // Only player attackers start the fight clock and produce rows;
        // monster damage is tracked above for boss-selection/reset purposes
        // only. Starting the clock on monster damage would let a boss
        // attacking the tank before players open fire dilute every row's DPS
        // with idle time.
        if d.attacker_kind != EntityKind::Player {
            return reason;
        }

        if self.fight_start_ms.is_none() {
            self.fight_start_ms = Some(d.timestamp_ms);
        }

        let cached = self.name_lookup(d.attacker_uid);
        let stats = self
            .players
            .entry(d.attacker_uid)
            .or_insert_with(|| PlayerStats::new(d.attacker_uid));
        if let Some(cached) = cached {
            if stats.name.is_none() {
                stats.name = cached.name;
            }
            if stats.class.is_none() {
                stats.class = cached.class;
            }
            if stats.ability_score.is_none() {
                stats.ability_score = cached.ability_score;
            }
            if stats.season_strength.is_none() {
                stats.season_strength = cached.season_strength;
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

        reason
    }

    /// Latches the fight end at `now_ms` if `uid` is a *recognized* boss
    /// (issue #78). The `tables::is_boss_monster` gate is what makes this
    /// signal usable: `recompute_boss` is a pure largest-max-hp heuristic, so
    /// without it the biggest trash mob in a pull would end the fight every
    /// time it died. An unrecognized (or not-yet-identified) monster falls
    /// back to the idle timeout, which is always safe.
    fn end_fight_on_boss_death(&mut self, uid: i64, now_ms: u64) {
        let recognized = self
            .enemies
            .get(&uid)
            .and_then(|e| e.monster_id)
            .is_some_and(tables::is_boss_monster);
        // Guarded on an in-progress fight so a kill packet arriving while no
        // fight is running (the tail of a pull the user just reset away)
        // can't leave a stale end time latched for the *next* fight to trip
        // over.
        if recognized && self.fight_start_ms.is_some() && self.fight_end_ms.is_none() {
            self.fight_end_ms = Some(now_ms);
        }
    }

    /// Counts one death for `target_uid`, debounced by `DEATH_DEBOUNCE_MS`
    /// against the last death counted for the same uid (issue #49). Lazily
    /// creates the target's `PlayerStats` entry — a player can die without
    /// ever having attacked (e.g. a healer or a fresh join), so this cannot
    /// rely on an entry the attacker-side path in `apply_damage` already
    /// made.
    fn record_death(&mut self, target_uid: i64, timestamp_ms: u64) {
        let stats = self
            .players
            .entry(target_uid)
            .or_insert_with(|| PlayerStats::new(target_uid));
        let debounced = stats
            .last_death_ms
            .is_some_and(|last| timestamp_ms.saturating_sub(last) < DEATH_DEBOUNCE_MS);
        if debounced {
            return;
        }
        stats.deaths += 1;
        stats.last_death_ms = Some(timestamp_ms);
    }

    fn apply_player(&mut self, p: &PlayerInfo) {
        let merged = self.name_upsert(
            p.uid,
            CachedAttrs {
                name: p.name.clone(),
                class: p.class,
                ability_score: p.ability_score,
                season_strength: p.season_strength,
                imagines: p.imagines,
            },
        );
        if let Some(stats) = self.players.get_mut(&p.uid) {
            apply_cached_attrs(stats, merged);
        } else if self.in_dungeon_scene()
            && merged.name.is_some()
            && self.preload_count < MAX_PRELOADED_PLAYERS
        {
            // issue #12: preload the roster. In a dungeon/raid instance the
            // only players in AOI range are the party, so eagerly creating a
            // zero-stat row here shows the whole group immediately instead
            // of only the players who have already hit or died. Gated
            // strictly on `in_dungeon_scene` — the same preload in town would
            // flood the meter with unrelated strangers passing through AOI
            // range. Also gated on `merged.name` (the *upserted* value, so a
            // cache hit counts too, per `name_upsert`): a row that would
            // render as "Player {uid}" is worse than no row at all. And
            // gated on `MAX_PRELOADED_PLAYERS` (issue #145 finding 3) as a
            // backstop against a misclassified scene preloading unbounded
            // rows.
            let mut stats = PlayerStats::new(p.uid);
            apply_cached_attrs(&mut stats, merged);
            self.players.insert(p.uid, stats);
            // issue #69/#12: no per-player log here by design (would flood
            // a raid); just tally, and let `prune_stale_preloads` emit one
            // sparse summary line when this scene ends.
            self.preload_count += 1;
        }
    }

    /// Whether the meter currently believes it's inside a dungeon/raid
    /// instance (issue #12), i.e. `scene_id` is known and resolves as a
    /// dungeon scene via `tables::is_dungeon_scene`. `None` (no `Scene`
    /// event seen yet this session, or cleared by `ServerChanged`) is
    /// treated as "not a dungeon" — preloading requires positive
    /// confirmation of AOI scope, never an absence of information.
    fn in_dungeon_scene(&self) -> bool {
        self.scene_id.is_some_and(tables::is_dungeon_scene)
    }

    /// Drops roster rows nobody has acted on yet: zero damage, zero hits,
    /// zero deaths (issue #12). Called on every real scene transition so a
    /// preloaded party member from the last dungeon (or a stray preload from
    /// just before a `Scene` event resolved) doesn't linger into the next
    /// one. Rows with any real activity are left alone — they still follow
    /// the existing reset rules (`reset`, `ResetReason`), not this.
    fn prune_stale_preloads(&mut self) {
        // Every row this drops is, by construction, a zero-stat row, and the
        // only path that creates one of those is the preload branch of
        // `apply_player` (a row from real damage/hits/deaths is never
        // all-zero). So `pruned` is exactly the untouched subset of this
        // scene's `preload_count`. Tallied inside the single `retain` pass
        // (issue #145 finding 5) rather than a separate `filter().count()`
        // pass first.
        let mut pruned = 0u32;
        self.players.retain(|_, p| {
            let stale = p.total_damage == 0 && p.hits == 0 && p.deaths == 0;
            if stale {
                pruned += 1;
            }
            !stale
        });
        // Sparse, transition-only diagnostic (issue #69/#12): one line per
        // scene left, never per player. `self.scene_id` is still the scene
        // being *left* here — `Meter::apply`'s `Scene` arm calls this before
        // overwriting it with the new id.
        if let Some(msg) = preload_summary_log(self.scene_id, self.preload_count, pruned) {
            log::info!("{msg}");
        }
        self.preload_count = 0;
    }

    fn apply_enemy_hp(&mut self, e: &EnemyHp) -> Option<ResetReason> {
        // `last_event_ms` is the DPS-window end and must reflect damage
        // only; enemy-HP sync/regen packets arriving after combat stops
        // would otherwise keep extending the denominator and decay DPS
        // toward zero with no combat happening.
        {
            let enemy = self.enemies.entry(e.uid).or_default();
            if let Some(curr) = e.curr_hp {
                enemy.curr_hp = Some(curr);
                // High-water mark, updated *before* `pct()` is read so a new
                // high reads as 100% of peak rather than as a stale ratio.
                // See `EnemyState::pct` for why the peak exists at all.
                enemy.peak_hp = Some(enemy.peak_hp.map_or(curr, |peak| peak.max(curr)));
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
            // issue #78: the boss's HP reaching 0 is the other end-of-fight
            // signal (the death packet can be missed; an HP sync to 0 is
            // hard to miss). Same recognized-boss gate as the death path.
            if self.fight_cfg.end_on_boss_death
                && self.enemies.get(&e.uid).and_then(|x| x.curr_hp) == Some(0)
            {
                self.end_fight_on_boss_death(e.uid, e.timestamp_ms);
            }

            let cooldown_ok = match self.last_reset_ms {
                Some(last) => e.timestamp_ms.saturating_sub(last) >= self.reset_cfg.cooldown_ms,
                None => true,
            };
            let should_reset = {
                let enemy = &self.enemies[&e.uid];
                check_hp_rollback(enemy, &self.reset_cfg)
            };
            // issue #78: while the last fight's stats are held, a boss HP bar
            // refilling (the corpse resyncing, or the next party pulling it)
            // must not clear them. The hold is only ever ended by combat the
            // *user* is part of, or by an explicit reset.
            let held = self.fight_ended_at(e.timestamp_ms).is_some();
            if should_reset {
                if cooldown_ok && !held {
                    self.reset(ResetReason::BossHpRollback, e.timestamp_ms);
                    return Some(ResetReason::BossHpRollback);
                }
                // The rollback shape was observed but suppressed (by the
                // cooldown gate, or by the post-fight hold). Latch it so the
                // same rollback can't re-fire the instant the cooldown
                // expires (it's level-triggered on `lowest_pct`, which only
                // clears inside `reset`).
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
    ///
    /// issue #76: `max_hp` is no longer *required*, only preferred. It
    /// arrives on the entity's `SyncNearEntities` appear packet; the HP
    /// deltas that follow carry `AttrHp` and `AttrId` but not `AttrMaxHp`.
    /// A meter started mid-pull therefore never learns the boss's `max_hp`
    /// at all, and demanding it left `boss_uid` — and so the header — empty
    /// for the whole fight. The reference trackers hit the same problem and
    /// each work around it rather than accepting the empty result: bpsr-logs
    /// keeps a `uid_to_monster_info` shadow map of `(monster_id, max_hp)`
    /// that outlives entity-map clears (`src-tauri/src/live/
    /// opcodes_process.rs:506-534`), and resonance-logs deliberately
    /// preserves boss HP attributes across segment switches "so the boss
    /// health bar remains visible" (`src-tauri/src/live/
    /// opcodes_process.rs:950-951`).
    ///
    /// Ranking keys, highest priority first (PR #100 review, findings 2 and
    /// 3):
    ///
    /// 1. **Recognized boss** (`tables::is_boss_monster`). A monster id in
    ///    the boss table is a far stronger signal than any HP number, so it
    ///    outranks everything else regardless of tier or HP. Without it,
    ///    within the `curr_hp`-only tier an *undamaged* trash add at 3M
    ///    outranks a real boss burned down to 2M of a 10M pool.
    /// 2. **HP tier**: a known `max_hp` (tier 1) outranks a `curr_hp`-only
    ///    enemy (tier 0), however large that current HP is — `max_hp` is the
    ///    real HP-side boss signal, while current HP is a moving number a
    ///    healthy trash mob can top while the boss sits at 10%. A
    ///    `max_hp` of `Some(0)` is treated as *unknown*, not as a known pool
    ///    of zero: otherwise a wire value that varint-decodes to 0 would win
    ///    tier 1 outright over a real mid-pull boss at 5M. This matches
    ///    `EnemyState::pct`, which already guards on `max > 0`.
    /// 3. **HP magnitude** within a tier, then **uid** to tie-break
    ///    deterministically: `HashMap` iteration order is unspecified, so
    ///    breaking ties on `hp` alone let `boss_uid` flip between calls for
    ///    two enemies sharing the same `max_hp`.
    ///
    /// An enemy with no HP of either kind is unrankable and stays out.
    fn recompute_boss(&mut self) {
        let previous_boss_uid = self.boss_uid;
        self.boss_uid = self
            .enemies
            .iter()
            .filter(|(_, e)| e.took_damage)
            .filter_map(|(uid, e)| {
                let recognized = u8::from(e.monster_id.is_some_and(tables::is_boss_monster));
                match (e.max_hp.filter(|max| *max > 0), e.curr_hp) {
                    (Some(max), _) => Some((*uid, recognized, 1u8, max)),
                    (None, Some(curr)) => Some((*uid, recognized, 0u8, curr)),
                    (None, None) => None,
                }
            })
            .max_by_key(|(uid, recognized, tier, hp)| (*recognized, *tier, *hp, *uid))
            .map(|(uid, _, _, _)| uid);

        let monster_id = self
            .boss_uid
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.monster_id);

        // issue #125: learn this dungeon's final boss by observation. No
        // upstream table maps a scene to its final boss, so the last genuine
        // boss (`is_boss_monster`) engaged in an actual dungeon scene
        // (`is_dungeon_scene`) is assumed to be it — see `scene_bosses`' doc
        // comment. Overwriting is correct and intended: a dungeon fought
        // through multiple bosses converges on the last one engaged, which
        // is the final boss. The `is_dungeon_scene` guard keeps a world boss
        // fought in an open-world zone from pinning its name to every later
        // visit to that town or field.
        if let (Some(id), Some(scene)) = (monster_id, self.scene_id)
            && tables::is_boss_monster(id)
            && tables::is_dungeon_scene(scene)
        {
            let previous = self.scene_bosses.get(&scene).copied();
            if previous != Some(id) {
                self.scene_bosses.insert(scene, id);
                if let Some(msg) = scene_boss_latch_log(previous, id, scene) {
                    log::info!("{msg}");
                }
            }
        }

        // Sparse, transition-only diagnostic (issue #69): `recompute_boss`
        // runs on every damage/enemy-hp event, so this must only log when
        // the winner actually changes — logging every call would reproduce
        // the #87 flood at boss-target granularity instead of attr-id
        // granularity.
        if self.boss_uid != previous_boss_uid
            && let Some(msg) = boss_transition_log(previous_boss_uid, self.boss_uid, monster_id)
        {
            log::info!("{msg}");
        }
    }

    /// Clears `players` and per-enemy `lowest_pct`; keeps `names`. Deaths
    /// are per-encounter (issue #49): `players.clear()` drops the whole
    /// `PlayerStats` entry per uid, taking `deaths`/`last_death_ms` with it,
    /// so no separate clearing step is needed here.
    ///
    /// Deliberately does **not** clear `scene_bosses` (issue #125): that map
    /// is session-lifetime by design, so the dungeon's final boss stays
    /// remembered across every reset a pull inside that dungeon can trigger
    /// (manual, boss-HP rollback) and across every later pull and later run
    /// of the same dungeon this session — clearing it here would defeat the
    /// entire point of latching it.
    pub fn reset(&mut self, reason: ResetReason, now_ms: u64) {
        // `reset` is itself already an event, never a per-snapshot poll, so
        // this is naturally sparse (issue #69) — no transition-only guard
        // needed the way scene/boss logging above requires one.
        log::info!("encounter: reset reason={reason:?}");
        self.players.clear();
        // issue #12/#145 finding 1: `players` just got cleared, so any
        // in-progress preload tally is now meaningless. The Scene and
        // ServerChanged arms of `apply` already zero this themselves (via
        // `prune_stale_preloads`, which also logs a summary first), but
        // `reset` is also reached from paths with no scene transition at
        // all — `BossHpRollback`, `NewFight`, and a `Manual` reset — so this
        // is the backstop that keeps `preload_count` in sync with the
        // cleared roster on every path, not just those two.
        self.preload_count = 0;
        for enemy in self.enemies.values_mut() {
            enemy.lowest_pct = None;
            enemy.took_damage = false;
        }
        self.fight_start_ms = None;
        // Every reset reason (manual, boss-HP rollback, server change, and
        // the next fight's first hit) drops the post-fight hold: the numbers
        // being held belong to the encounter that is being cleared.
        self.fight_end_ms = None;
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

        // issue #78: once the fight has ended the snapshot is rendered as of
        // the fight's end, not the caller's clock, so the elapsed timer stops
        // advancing and the display holds the last pull's numbers until the
        // next fight starts.
        let effective_now_ms = self.fight_ended_at(now_ms).unwrap_or(now_ms);

        let display_duration_ms = match self.fight_start_ms {
            Some(start) => effective_now_ms.saturating_sub(start).max(1),
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
                    season_strength: p.season_strength,
                    imagines: p.imagines.unwrap_or_default(),
                    damage: p.total_damage,
                    dps,
                    share_pct,
                    crit_pct: p.crit_pct(),
                    lucky_pct: p.lucky_pct(),
                    hits: p.hits,
                    deaths: p.deaths,
                }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.damage));

        let total_dps = total_damage as f64 / (dps_duration_ms as f64 / 1000.0);

        let boss_monster_id = self
            .boss_uid
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.monster_id);
        // issue #42: `recompute_boss` prefers a recognized boss but still
        // falls back to an HP heuristic when no monster in the pull is in the
        // table, so `boss_monster_id` alone can't tell a real boss from a big
        // trash mob. Gate the *display* fields
        // on `tables::is_boss_monster`; `boss_monster_id` itself stays
        // populated for every pull since it's real data, not a display
        // choice.
        let is_boss = boss_monster_id.is_some_and(tables::is_boss_monster);
        // issue #125: the dungeon's remembered final boss, if `scene_id` is
        // known and a boss has been latched for it — see `scene_bosses`' doc
        // comment. Independent of `boss_monster_id`/`is_boss` above, which
        // stay the raw facts about the currently-selected target; this is
        // what `encounter_title` prefers over them.
        let scene_boss_name = self
            .scene_id
            .and_then(|scene| self.scene_bosses.get(&scene))
            .and_then(|&id| tables::monster_name(id));
        let encounter = EncounterInfo {
            boss_monster_id,
            boss_name: if is_boss {
                boss_monster_id.and_then(tables::monster_name)
            } else {
                None
            },
            is_boss,
            scene_id: self.scene_id,
            scene_name: self.scene_id.and_then(tables::scene_name),
            scene_boss_name,
        };

        Snapshot {
            duration_ms: display_duration_ms,
            total_damage,
            total_dps,
            rows,
            encounter,
        }
    }

    /// Whether a fight's stats are on the board — true both while it is
    /// running and while an ended fight is being held (issue #78). Use
    /// [`Meter::fight_state`] to tell those two apart.
    pub fn is_active(&self) -> bool {
        self.fight_start_ms.is_some()
    }
}

/// Builds the "scene changed" diagnostic line (issue #69), or `None` when
/// `new_scene_id` matches `previous` — the transition-only guard that keeps
/// this out of the #87-style flood. `new_scene_id` is `Option<u32>` so this
/// can also represent a clear-to-`None` transition (the `ServerChanged` arm
/// of `Meter::apply`, mirroring how [`boss_transition_log`] handles a
/// cleared boss target) rather than only ever moving between two concrete
/// scenes. Split out from `Meter::apply` as a pure function so the decision
/// (log or not, and what) is unit-testable without a log-capturing harness.
fn scene_transition_log(previous: Option<u32>, new_scene_id: Option<u32>) -> Option<String> {
    if previous == new_scene_id {
        return None;
    }
    Some(match new_scene_id {
        None => "encounter: scene cleared".to_string(),
        Some(id) => match tables::scene_name(id) {
            Some(name) => format!("encounter: scene changed to id={id} name={name}"),
            None => format!("encounter: scene changed to id={id} name=<unresolved>"),
        },
    })
}

/// Builds the "preload summary" diagnostic line (issue #12/#69) for the
/// scene being *left*, or `None` when that scene doesn't resolve as a
/// dungeon/raid instance via `tables::is_dungeon_scene` — no preloads can
/// exist outside one, so the line would be pure noise. `preloaded` is the
/// scene's `Meter::preload_count`; `pruned` is how many of those rows
/// `prune_stale_preloads` just dropped as untouched, so `preloaded - pruned`
/// is how many went on to record real activity. Never includes a player
/// name or uid — only counts, since these logs get shared for debugging.
/// Pure, like [`scene_transition_log`], for the same testability reason.
fn preload_summary_log(scene_id: Option<u32>, preloaded: u32, pruned: u32) -> Option<String> {
    let id = scene_id.filter(|id| tables::is_dungeon_scene(*id))?;
    let active = preloaded.saturating_sub(pruned);
    Some(format!(
        "encounter: scene={id} preload summary: preloaded={preloaded} active={active} pruned={pruned}"
    ))
}

/// Builds the "boss target changed" diagnostic line (issue #69), or `None`
/// when `new_uid` matches `previous_uid` — `recompute_boss` runs on every
/// damage/enemy-hp event, so without this guard the line would reproduce
/// the #87 flood at boss-target granularity. Pure, like
/// [`scene_transition_log`], for the same testability reason.
fn boss_transition_log(
    previous_uid: Option<i64>,
    new_uid: Option<i64>,
    monster_id: Option<u32>,
) -> Option<String> {
    if previous_uid == new_uid {
        return None;
    }
    Some(match new_uid {
        None => "encounter: boss target cleared".to_string(),
        Some(uid) => match monster_id {
            Some(id) => {
                let recognized = tables::is_boss_monster(id);
                match tables::monster_name(id) {
                    Some(name) => format!(
                        "encounter: boss target changed to uid={uid} monster_id={id} recognized_boss={recognized} name={name}"
                    ),
                    None => format!(
                        "encounter: boss target changed to uid={uid} monster_id={id} recognized_boss={recognized} name=<unresolved>"
                    ),
                }
            }
            None => format!("encounter: boss target changed to uid={uid} monster_id=<unknown>"),
        },
    })
}

/// Builds the "dungeon final boss learned/changed" diagnostic line (issue
/// #125), or `None` when `new_monster_id` matches `previous` — `recompute_boss`
/// only calls this (and only re-inserts into `scene_bosses`) when the winner
/// differs from what's already latched, but this guard is kept as a second
/// line of defense against relogging an unchanged boss. Pure, like
/// [`scene_transition_log`]/[`boss_transition_log`], for the same
/// testability reason.
fn scene_boss_latch_log(
    previous: Option<u32>,
    new_monster_id: u32,
    scene_id: u32,
) -> Option<String> {
    if previous == Some(new_monster_id) {
        return None;
    }
    Some(match tables::monster_name(new_monster_id) {
        Some(name) => format!(
            "encounter: scene={scene_id} final boss latched monster_id={new_monster_id} name={name}"
        ),
        None => format!(
            "encounter: scene={scene_id} final boss latched monster_id={new_monster_id} name=<unresolved>"
        ),
    })
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
            season_strength: None,
            imagines: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].name, "Foo");
        assert_eq!(snap.rows[0].class, Some(Class::Stormblade));
    }

    fn player_info(uid: i64, name: &str) -> ProtocolEvent {
        ProtocolEvent::Player(PlayerInfo {
            uid,
            name: Some(name.to_string()),
            class: Some(Class::Stormblade),
            ability_score: None,
            season_strength: None,
            imagines: None,
        })
    }

    // -- issue #12: dungeon-gated name preload -----------------------------

    #[test]
    fn player_event_in_dungeon_preloads_a_zero_stat_row() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        }); // real dungeon id
        m.apply(&player_info(1, "Alice"));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].uid, 1);
        assert_eq!(snap.rows[0].name, "Alice");
        assert_eq!(snap.rows[0].damage, 0);
        assert_eq!(snap.rows[0].hits, 0);
        assert!(!m.is_active(), "a preload must not start the fight clock");
    }

    #[test]
    fn player_event_in_town_does_not_preload() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene { level_map_id: 8 }); // Asterleeds, not a dungeon
        m.apply(&player_info(1, "Alice"));
        let snap = m.snapshot(2000);
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn player_event_in_gloomy_depths_does_not_preload() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene { level_map_id: 92 }); // Gloomy Depths, not a dungeon
        m.apply(&player_info(1, "Alice"));
        let snap = m.snapshot(2000);
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn player_event_with_no_scene_does_not_preload() {
        let mut m = Meter::new();
        m.apply(&player_info(1, "Alice"));
        let snap = m.snapshot(2000);
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn preloaded_row_accumulates_damage_without_double_counting_or_losing_name() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        m.apply(&player_info(1, "Alice"));
        m.apply(&dmg(1, 500, 1000));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].name, "Alice");
        assert_eq!(snap.rows[0].damage, 500);
        assert_eq!(snap.rows[0].hits, 1);
        assert_eq!(snap.total_damage, 500);
    }

    #[test]
    fn share_and_dps_stay_finite_with_mixed_preload_and_real_rows() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        // Two preloads, neither ever deals damage.
        m.apply(&player_info(1, "Alice"));
        m.apply(&player_info(2, "Bob"));
        // One real attacker.
        m.apply(&dmg(3, 1000, 1000));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows.len(), 3);
        let total_share: f32 = snap.rows.iter().map(|r| r.share_pct).sum();
        for row in &snap.rows {
            assert!(row.share_pct.is_finite());
            assert!(row.dps.is_finite());
            assert!(row.crit_pct.is_finite());
            assert!(row.lucky_pct.is_finite());
        }
        assert!((total_share - 100.0).abs() < 0.01);
        // Zero-damage preloads sort to the bottom, stably, behind the real
        // attacker.
        assert_eq!(snap.rows[0].uid, 3);
    }

    #[test]
    fn leaving_a_dungeon_drops_untouched_preloads_but_keeps_active_rows() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        m.apply(&player_info(1, "Alice")); // never acts
        m.apply(&player_info(2, "Bob"));
        m.apply(&dmg(2, 200, 1000)); // Bob deals damage
        // Leave the dungeon for a different scene.
        m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].uid, 2);
        assert_eq!(snap.rows[0].name, "Bob");
        assert_eq!(snap.rows[0].damage, 200);
    }

    #[test]
    fn dungeon_to_different_dungeon_drops_untouched_preloads() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        m.apply(&player_info(1, "Alice")); // never acts
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 31101,
        }); // a different dungeon
        let snap = m.snapshot(2000);
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn preload_count_increments_only_via_the_preload_path() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        assert_eq!(m.preload_count, 0);
        m.apply(&player_info(1, "Alice")); // preload
        assert_eq!(m.preload_count, 1);
        // A second Player event for the same uid updates the existing row,
        // not a new preload.
        m.apply(&player_info(1, "Alice"));
        assert_eq!(m.preload_count, 1);
        // Real damage for an already-preloaded uid doesn't touch the counter.
        m.apply(&dmg(1, 100, 1000));
        assert_eq!(m.preload_count, 1);
    }

    #[test]
    fn preload_count_does_not_increment_outside_a_dungeon_scene() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene { level_map_id: 8 }); // town, not a dungeon
        m.apply(&player_info(1, "Alice"));
        assert_eq!(m.preload_count, 0);
    }

    #[test]
    fn preload_count_resets_on_scene_entry() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        m.apply(&player_info(1, "Alice"));
        m.apply(&player_info(2, "Bob"));
        assert_eq!(m.preload_count, 2);
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 31101,
        }); // a different dungeon
        assert_eq!(m.preload_count, 0);
        m.apply(&player_info(3, "Cara"));
        assert_eq!(m.preload_count, 1);
    }

    #[test]
    fn preload_accounting_balances_after_a_mixed_scenario() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        // Five preloaded party members; two go on to deal real damage.
        for (uid, name) in [
            (1, "Alice"),
            (2, "Bob"),
            (3, "Cara"),
            (4, "Dan"),
            (5, "Eve"),
        ] {
            m.apply(&player_info(uid, name));
        }
        let preloaded = m.preload_count;
        assert_eq!(preloaded, 5);
        m.apply(&dmg(2, 100, 1000));
        m.apply(&dmg(4, 50, 1000));
        // Leave the dungeon: `prune_stale_preloads` runs, drops the three
        // untouched rows, and resets the counter for the new scene.
        m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
        let snap = m.snapshot(2000);
        let active = snap.rows.len() as u32;
        let pruned = preloaded - active;
        assert_eq!(active, 2);
        assert_eq!(pruned, 3);
        assert_eq!(
            preloaded,
            active + pruned,
            "preloaded must equal still-active + pruned"
        );
        assert_eq!(m.preload_count, 0);
    }

    /// issue #145 findings 1/2: `Meter::reset` used to clear `players`
    /// without touching `preload_count`, so a reset that isn't a scene
    /// transition (a `BossHpRollback` mid-dungeon, or a `Manual` reset) left
    /// the counter carrying the previous pull's preloads into the next one.
    /// Preloads once, resets mid-scene (no `Scene` event in between),
    /// preloads again, then leaves the dungeon and checks the
    /// `preloaded = active + pruned` invariant still holds — same shape as
    /// `preload_accounting_balances_after_a_mixed_scenario`, but with a
    /// reset spliced into the middle of the scene. Fails against the
    /// pre-fix code: `preload_count` would read 5 (both preload batches
    /// counted) while only the second batch's 3 rows are still in
    /// `players`, so `pruned` comes out negative-equivalent (wraps as a
    /// `u32`) instead of 2.
    #[test]
    fn preload_count_stays_in_sync_across_a_mid_dungeon_reset() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        // First pull: two preloads, no `Scene` event before the reset below.
        m.apply(&player_info(1, "Alice"));
        m.apply(&player_info(2, "Bob"));
        assert_eq!(m.preload_count, 2);
        m.reset(ResetReason::BossHpRollback, 1000);
        assert_eq!(m.preload_count, 0);
        assert!(m.snapshot(1000).rows.is_empty());
        // Second pull, same scene: three fresh preloads, one goes on to hit.
        m.apply(&player_info(3, "Cara"));
        m.apply(&player_info(4, "Dan"));
        m.apply(&player_info(5, "Eve"));
        let preloaded = m.preload_count;
        assert_eq!(preloaded, 3);
        m.apply(&dmg(3, 100, 2000));
        // Leave the dungeon: prune should only see this pull's preloads.
        m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
        let snap = m.snapshot(3000);
        let active = snap.rows.len() as u32;
        let pruned = preloaded - active;
        assert_eq!(active, 1);
        assert_eq!(pruned, 2);
        assert_eq!(
            preloaded,
            active + pruned,
            "preloaded must equal still-active + pruned even across a mid-dungeon reset"
        );
        assert_eq!(m.preload_count, 0);
    }

    /// issue #145 finding 4: `ServerChanged` used to reset straight away
    /// without pruning first, leaving `preload_count` stale (and skipping
    /// the summary log) the same way an un-fixed `BossHpRollback` would.
    /// Mirrors `preload_count_stays_in_sync_across_a_mid_dungeon_reset`, but
    /// with the scene ending via `ServerChanged` instead of a same-scene
    /// reset.
    #[test]
    fn server_changed_prunes_preloads_like_a_real_scene_change() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        m.apply(&player_info(1, "Alice"));
        m.apply(&player_info(2, "Bob"));
        assert_eq!(m.preload_count, 2);
        m.apply(&dmg(1, 100, 1000));
        m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 2000 });
        assert_eq!(m.preload_count, 0);
        // The one row with real activity doesn't survive `ServerChanged`
        // either — `reset` still clears `players` outright — but the
        // counter accounting is what this test is about.
        assert!(m.snapshot(2000).rows.is_empty());
    }

    /// issue #145 finding 3: defensive cap on preloads per scene, guarding
    /// against a misclassified dungeon scene preloading unboundedly.
    /// Preloads well past `MAX_PRELOADED_PLAYERS` and asserts both the
    /// counter and the roster stop growing at the cap, which sits
    /// comfortably above the largest real raid this meter supports (20
    /// players — see `preloading_a_full_20_player_raid_snapshots_cleanly`).
    #[test]
    fn preload_count_is_capped_per_scene() {
        assert!(
            MAX_PRELOADED_PLAYERS > 20,
            "cap must comfortably exceed the largest real raid"
        );
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        for uid in 1..=(MAX_PRELOADED_PLAYERS as i64 + 10) {
            m.apply(&player_info(uid, &format!("Player{uid}")));
        }
        assert_eq!(m.preload_count, MAX_PRELOADED_PLAYERS);
        assert_eq!(m.players.len() as u32, MAX_PRELOADED_PLAYERS);
    }

    /// Raid-scale sanity check: up to 20 simultaneous party members (a full
    /// raid, not just a 5-player dungeon party) is 4x anything the earlier
    /// preload tests exercised. Preloads 20 distinct named players, mixes in
    /// a couple of real-damage rows among them, and asserts every row
    /// snapshots without panicking and with finite, sane stats.
    #[test]
    fn preloading_a_full_20_player_raid_snapshots_cleanly() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Scene {
            level_map_id: 40001,
        });
        for uid in 1..=20i64 {
            m.apply(&player_info(uid, &format!("Player{uid}")));
        }
        // A couple of real attackers among the 20 preloads.
        m.apply(&dmg(3, 700, 1000));
        m.apply(&dmg(11, 300, 1000));

        let snap = m.snapshot(2000);
        assert_eq!(snap.rows.len(), 20);

        let total_share: f32 = snap.rows.iter().map(|r| r.share_pct).sum();
        for row in &snap.rows {
            assert!(row.share_pct.is_finite() && row.share_pct >= 0.0);
            assert!(row.dps.is_finite() && row.dps >= 0.0);
            assert!(row.crit_pct.is_finite());
            assert!(row.lucky_pct.is_finite());
        }
        assert!((total_share - 100.0).abs() < 0.01);

        // Stable sort: the two real-damage rows lead, ordered by damage;
        // the 18 zero-damage preloads trail behind them, and their relative
        // (insertion) order among themselves is otherwise unconstrained by
        // this assertion — only that they're all *after* the real rows.
        assert_eq!(snap.rows[0].uid, 3);
        assert_eq!(snap.rows[1].uid, 11);
        for row in &snap.rows[2..] {
            assert_eq!(row.damage, 0);
        }
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
            season_strength: None,
            imagines: None,
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
            season_strength: None,
            imagines: None,
        }));
        m.apply(&dmg(3, 100, 0));
        m.reset(ResetReason::Manual, 1000);
        m.apply(&dmg(3, 50, 2000));
        let snap = m.snapshot(3000);
        assert_eq!(snap.rows[0].ability_score, Some(1000));
    }

    /// A `PlayerInfo` carrying `Some([Some(id), None])` surfaces on the row
    /// (issue #33). `bpsr-meter` treats the id as opaque — it never
    /// interprets it, only threads it through.
    #[test]
    fn player_info_imagines_reach_row() {
        let mut m = Meter::new();
        m.apply(&dmg(9, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), None]),
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].imagines, [Some(3905), None]);
    }

    /// `imagines: None` means no `0x74` packet has been seen *this time* —
    /// it must not clobber a previously cached pair, mirroring
    /// `ability_score`'s "`Some` overwrites, `None` preserves" merge rule.
    #[test]
    fn imagines_none_does_not_clobber_the_cached_pair() {
        let mut m = Meter::new();
        m.apply(&dmg(9, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), Some(102640)]),
        }));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].imagines, [Some(3905), Some(102640)]);
    }

    /// `Some([None, None])` means a packet *was* seen and this player has no
    /// known Imagines — unlike bare `None`, this does overwrite ("live
    /// wins").
    #[test]
    fn imagines_some_none_none_overwrites_the_cached_pair() {
        let mut m = Meter::new();
        m.apply(&dmg(9, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), Some(102640)]),
        }));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([None, None]),
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].imagines, [None, None]);
    }

    /// A player with no Imagine packet at all snapshots as `[None, None]`,
    /// not a missing/default row.
    #[test]
    fn no_imagine_packet_snapshots_as_empty_slots() {
        let mut m = Meter::new();
        m.apply(&dmg(9, 100, 1000));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].imagines, [None, None]);
    }

    #[test]
    fn player_info_season_strength_reaches_row() {
        let mut m = Meter::new();
        m.apply(&dmg(8, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 8,
            name: None,
            class: None,
            ability_score: None,
            season_strength: Some(12_345),
            imagines: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].season_strength, Some(12_345));
    }

    #[test]
    fn season_strength_survives_reset_like_name_and_class() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            uid: 4,
            name: Some("Foo".to_string()),
            class: None,
            ability_score: None,
            season_strength: Some(999),
            imagines: None,
        }));
        m.apply(&dmg(4, 100, 0));
        m.reset(ResetReason::Manual, 1000);
        m.apply(&dmg(4, 50, 2000));
        let snap = m.snapshot(3000);
        assert_eq!(snap.rows[0].season_strength, Some(999));
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
        // Both hits inside the fight-end idle window (issue #78), so this is
        // one fight: a gap longer than `FightConfig::idle_timeout_ms` would
        // legitimately be two.
        m.apply(&dmg(1, 2500, 0));
        m.apply(&dmg(1, 2500, 5_000));
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

    mod deaths {
        use super::*;

        fn death_hit(attacker_uid: i64, target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid,
                attacker_kind: EntityKind::Player,
                target_uid,
                target_kind: EntityKind::Player,
                value: 100,
                is_dead: true,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        #[test]
        fn dead_player_target_increments_the_targets_death_count_not_the_attackers() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 1000));
            let snap = m.snapshot(2000);
            let row = |uid| snap.rows.iter().find(|r| r.uid == uid).unwrap();
            assert_eq!(row(2).deaths, 1);
            assert_eq!(row(1).deaths, 0);
        }

        #[test]
        fn is_dead_on_a_monster_target_increments_nobody() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: 10,
                target_kind: EntityKind::Monster,
                value: 100,
                is_dead: true,
                timestamp_ms: 1000,
                ..Default::default()
            }));
            let snap = m.snapshot(2000);
            assert_eq!(snap.rows.len(), 1);
            assert_eq!(snap.rows[0].deaths, 0);
        }

        #[test]
        fn duplicate_death_within_debounce_window_counts_once() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 1000));
            m.apply(&death_hit(1, 2, 1000 + DEATH_DEBOUNCE_MS - 1));
            let snap = m.snapshot(2000);
            assert_eq!(snap.rows.iter().find(|r| r.uid == 2).unwrap().deaths, 1);
        }

        #[test]
        fn death_outside_debounce_window_counts_again() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 1000));
            m.apply(&death_hit(1, 2, 1000 + DEATH_DEBOUNCE_MS));
            let snap = m.snapshot(2000 + DEATH_DEBOUNCE_MS);
            assert_eq!(snap.rows.iter().find(|r| r.uid == 2).unwrap().deaths, 2);
        }

        #[test]
        fn heal_typed_dead_player_event_still_records_death() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: 2,
                target_kind: EntityKind::Player,
                value: 100,
                is_heal: true,
                is_dead: true,
                timestamp_ms: 1000,
                ..Default::default()
            }));
            let snap = m.snapshot(2000);
            let row = snap.rows.iter().find(|r| r.uid == 2).unwrap();
            assert_eq!(row.deaths, 1);
            assert_eq!(row.damage, 0);
            assert_eq!(row.hits, 0);
        }

        #[test]
        fn reset_clears_deaths() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 1000));
            m.reset(ResetReason::Manual, 2000);
            m.apply(&dmg(2, 50, 3000));
            let snap = m.snapshot(4000);
            assert_eq!(snap.rows.iter().find(|r| r.uid == 2).unwrap().deaths, 0);
        }
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
                season_strength: None,
                imagines: None,
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
                season_strength: None,
                imagines: None,
            }));
            m.apply(&dmg(5, 100, 1000));

            let snap = m.snapshot(2000);
            assert_eq!(snap.rows[0].name, "Renamed");
            assert_eq!(snap.rows[0].class, Some(Class::Marksman));
        }

        /// Issue #37: an Imagine transform decodes (in `bpsr-protocol`) to a
        /// `PlayerInfo` with `class: None`, never `Some(Class::Unknown)`. This
        /// regression test documents that the meter's existing "`Some`
        /// overwrites, `None` preserves" merge rule (`name_upsert` /
        /// `apply_player` above) already handles that correctly and needs no
        /// Imagine-specific logic of its own — it passes without any change
        /// to this file, unlike the `bpsr-protocol` tests for this issue
        /// which must go red first.
        #[test]
        fn class_none_packet_preserves_a_previously_known_class() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 5,
                name: Some("Ren".to_string()),
                class: Some(Class::Stormblade),
                ability_score: None,
                season_strength: None,
                imagines: None,
            }));
            m.apply(&dmg(5, 100, 1000));

            // A simulated Imagine-transform packet: profession id decoded to
            // no class at all (see `bpsr_protocol::pb::class_of_profession_id`).
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 5,
                name: None,
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
            }));

            let snap = m.snapshot(2000);
            assert_eq!(snap.rows[0].name, "Ren");
            assert_eq!(snap.rows[0].class, Some(Class::Stormblade));
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
                season_strength: None,
                imagines: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 2,
                name: Some("B".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 3,
                name: Some("C".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
            }));
            // Re-touch uid 1 so it becomes the most recently used, ahead of
            // 3 and 2 (in that order).
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("A".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
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
                season_strength: None,
                imagines: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 2,
                name: Some("Second".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
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
                season_strength: None,
                imagines: None,
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

        /// An enemy seen only through HP deltas: `AttrHp` but no `AttrMaxHp`,
        /// the shape a meter started mid-pull gets (issue #76).
        fn curr_hp_only(uid: i64, curr: u64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: ts,
            })
        }

        /// PR #100 review, finding 1: resolving a boss from `curr_hp` alone
        /// must not cost it wipe detection. `pct()` used to need both HP
        /// fields, so `check_hp_rollback` short-circuited to `false` and the
        /// wiped attempt's damage kept piling into the next pull until the
        /// idle timeout fired — in exactly the mid-pull-join scenario issue
        /// #76 exists to support.
        #[test]
        fn curr_hp_only_boss_still_fires_the_wipe_rollback_reset() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.apply(&curr_hp_only(10, 5_000_000, 0)), None);
            // Burned to 20% of the highest HP ever observed.
            assert_eq!(m.apply(&curr_hp_only(10, 1_000_000, 100)), None);
            // Wipe: the bar snaps back up to (at least) that peak.
            let r = m.apply(&curr_hp_only(10, 5_000_000, 200));
            assert_eq!(r, Some(ResetReason::BossHpRollback));
            // And the wiped attempt's damage is gone rather than carrying
            // into the next pull.
            assert_eq!(m.snapshot(1_000).total_damage, 0);
        }

        #[test]
        fn curr_hp_only_boss_being_burned_down_does_not_fire_a_reset() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&curr_hp_only(10, 5_000_000, 0));
            m.apply(&curr_hp_only(10, 1_000_000, 100));
            // A partial recovery well short of the peak is just healing.
            assert_eq!(m.apply(&curr_hp_only(10, 2_000_000, 200)), None);
            assert_eq!(m.apply(&curr_hp_only(10, 500_000, 300)), None);
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
                season_strength: None,
                imagines: None,
            }));
            m.apply(&dmg(1, 100, 0));
            m.reset(ResetReason::Manual, 1000);
            assert!(m.players.is_empty());
            m.apply(&dmg(1, 50, 2000));
            let snap = m.snapshot(3000);
            assert_eq!(snap.rows[0].name, "Foo");
        }
    }

    /// Issue #78: a fight that has ended holds its stats on screen until the
    /// next fight actually starts.
    mod fight_end {
        use super::*;

        /// The default idle window, as a plain value so the cases below read
        /// as "just inside / just outside the window".
        fn idle() -> u64 {
            FightConfig::default().idle_timeout_ms
        }

        /// A player hit on monster `uid`, optionally the killing blow.
        fn boss_hit(uid: i64, ts: u64, is_dead: bool) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 100,
                is_dead,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        fn hp(uid: i64, curr: u64, monster_id: Option<u32>, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(100),
                monster_id,
                timestamp_ms: ts,
            })
        }

        #[test]
        fn fight_is_active_while_damage_keeps_arriving() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.fight_state(1_000 + idle() - 1), FightState::Active);
        }

        #[test]
        fn fight_ends_after_the_idle_window() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.fight_state(1_000 + idle()), FightState::Ended);
        }

        #[test]
        fn no_fight_at_all_stays_idle() {
            let m = Meter::new();
            assert_eq!(m.fight_state(600_000), FightState::Idle);
        }

        #[test]
        fn stats_and_elapsed_timer_are_held_while_ended() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            m.apply(&dmg(1, 5_000, 5_000));

            // Two snapshots five minutes apart, both after the fight ended.
            let first = m.snapshot(5_000 + idle());
            let later = m.snapshot(600_000);

            assert_eq!(first.duration_ms, 5_000);
            assert_eq!(later.duration_ms, first.duration_ms);
            assert_eq!(later.total_damage, 10_000);
            assert_eq!(later.rows.len(), 1);
            assert!((later.rows[0].dps - first.rows[0].dps).abs() < 0.01);
        }

        #[test]
        fn tick_latches_the_end_at_the_last_damage() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.tick(1_000 + idle()), FightState::Ended);

            // Re-widening the idle window must not un-end a latched fight.
            m.set_fight_config(FightConfig {
                idle_timeout_ms: 600_000,
                ..FightConfig::default()
            });
            assert_eq!(m.fight_state(1_000 + idle()), FightState::Ended);
            assert_eq!(m.snapshot(600_000).duration_ms, 1);
        }

        #[test]
        fn tick_reports_active_and_idle_without_latching() {
            let mut m = Meter::new();
            assert_eq!(m.tick(1_000), FightState::Idle);
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.tick(2_000), FightState::Active);
            assert_eq!(m.fight_state(600_000), FightState::Ended);
        }

        #[test]
        fn a_zero_idle_timeout_disables_idle_detection() {
            let mut m = Meter::with_fight_config(FightConfig {
                idle_timeout_ms: 0,
                ..FightConfig::default()
            });
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.fight_state(600_000), FightState::Active);
        }

        #[test]
        fn new_damage_after_the_hold_clears_and_starts_a_fresh_fight() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));

            let reason = m.apply(&dmg(1, 300, 100_000));
            assert_eq!(reason, Some(ResetReason::NewFight));

            let snap = m.snapshot(101_000);
            assert_eq!(snap.total_damage, 300, "old fight's damage must be gone");
            assert_eq!(m.fight_state(101_000), FightState::Active);
            // The new fight's clock is anchored to its own first hit.
            assert_eq!(snap.duration_ms, 1_000);
        }

        #[test]
        fn damage_inside_the_idle_window_does_not_reset() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            let reason = m.apply(&dmg(1, 5_000, idle() - 1));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(idle()).total_damage, 10_000);
        }

        #[test]
        fn a_monster_swinging_at_a_player_does_not_end_the_hold() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));

            // A mob aggroes the player in town long after the pull ended.
            let reason = m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 99,
                attacker_kind: EntityKind::Monster,
                target_uid: 1,
                target_kind: EntityKind::Player,
                value: 200,
                timestamp_ms: 100_000,
                ..Default::default()
            }));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(101_000).total_damage, 5_000);
            assert_eq!(m.fight_state(101_000), FightState::Ended);
        }

        #[test]
        fn a_heal_does_not_end_the_hold() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            let reason = m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                value: 400,
                is_heal: true,
                timestamp_ms: 100_000,
                ..Default::default()
            }));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(101_000).total_damage, 5_000);
        }

        #[test]
        fn manual_reset_clears_immediately_from_the_ended_state() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            assert_eq!(m.tick(100_000), FightState::Ended);

            m.reset(ResetReason::Manual, 100_000);

            let snap = m.snapshot(101_000);
            assert!(snap.rows.is_empty());
            assert_eq!(snap.total_damage, 0);
            assert_eq!(snap.duration_ms, 0);
            assert_eq!(m.fight_state(101_000), FightState::Idle);
        }

        #[test]
        fn server_change_clears_from_the_ended_state() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            assert_eq!(m.tick(100_000), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 100_000,
            });
            assert_eq!(reason, Some(ResetReason::ServerChange));
            assert!(m.snapshot(101_000).rows.is_empty());
            assert_eq!(m.fight_state(101_000), FightState::Idle);
        }

        #[test]
        fn a_recognized_boss_dying_ends_the_fight_immediately() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 0)); // 103 = a catalogued boss
            m.apply(&boss_hit(10, 1_000, true));

            assert_eq!(m.fight_state(1_100), FightState::Ended);
            assert_eq!(m.snapshot(60_000).duration_ms, 1_000);
        }

        #[test]
        fn a_trash_mob_dying_does_not_end_the_fight() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(10_900), 0)); // named, but not a boss
            m.apply(&boss_hit(10, 1_000, true));

            assert_eq!(m.fight_state(1_100), FightState::Active);
        }

        #[test]
        fn an_unidentified_monster_dying_does_not_end_the_fight() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, None, 0));
            m.apply(&boss_hit(10, 1_000, true));

            assert_eq!(m.fight_state(1_100), FightState::Active);
        }

        #[test]
        fn boss_death_detection_can_be_disabled() {
            let mut m = Meter::with_fight_config(FightConfig {
                end_on_boss_death: false,
                ..FightConfig::default()
            });
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 0));
            m.apply(&boss_hit(10, 1_000, true));

            assert_eq!(m.fight_state(1_100), FightState::Active);
        }

        #[test]
        fn boss_hp_reaching_zero_ends_the_fight() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 500));
            m.apply(&hp(10, 0, Some(103), 1_000));

            assert_eq!(m.fight_state(1_100), FightState::Ended);
        }

        #[test]
        fn a_second_zero_hp_sync_does_not_drift_the_latched_end() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 500));
            m.apply(&hp(10, 0, Some(103), 1_000));

            let held = m.snapshot(60_000);
            assert_eq!(held.duration_ms, 1_000);

            // A duplicate zero-HP sync for the same, already-dead boss,
            // arriving long after the fight was latched as ended: the
            // latch must be once-only, or this re-enters
            // `end_fight_on_boss_death` and drags `fight_end_ms` (and thus
            // the frozen duration) forward.
            m.apply(&hp(10, 0, Some(103), 500_000));

            let later = m.snapshot(600_000);
            assert_eq!(later.duration_ms, held.duration_ms);
        }

        #[test]
        fn a_boss_hp_rollback_cannot_clear_a_held_fight() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 100, Some(103), 0));
            m.apply(&hp(10, 55, Some(103), 100));

            // The pull ends; the meter is holding its stats.
            assert_eq!(m.tick(100_000), FightState::Ended);

            // The corpse (or the next party's pull) refills the HP bar: the
            // classic rollback shape, which must not wipe the held numbers.
            let reason = m.apply(&hp(10, 95, Some(103), 120_000));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(121_000).total_damage, 100);
            assert_eq!(m.fight_state(121_000), FightState::Ended);
        }

        #[test]
        fn a_boss_hp_rollback_still_resets_during_a_live_fight() {
            // Guards the change above from over-reaching: an in-progress
            // wipe/rollback must keep resetting exactly as before.
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 100, None, 0));
            m.apply(&hp(10, 55, None, 100));
            let reason = m.apply(&hp(10, 95, None, 200));
            assert_eq!(reason, Some(ResetReason::BossHpRollback));
        }

        #[test]
        fn the_next_fight_after_a_boss_kill_clears_the_held_stats() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 0));
            m.apply(&boss_hit(10, 1_000, true));
            assert_eq!(m.fight_state(1_100), FightState::Ended);

            // Next pull, only two seconds later — well inside the idle
            // window, but the fight already ended, so this starts a new one.
            let reason = m.apply(&dmg(1, 700, 3_000));
            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(4_000).total_damage, 700);
        }

        #[test]
        fn names_survive_the_new_fight_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
            }));
            m.apply(&dmg(1, 100, 0));
            m.apply(&dmg(1, 100, 100_000));
            assert_eq!(m.snapshot(101_000).rows[0].name, "Foo");
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
        fn boss_name_resolves_for_a_known_boss_id() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert_eq!(snap.encounter.boss_name, Some("Rathalos"));
            assert!(snap.encounter.is_boss);
        }

        /// issue #112: the curated `BOSS_MONSTER_IDS` list jumped straight
        /// from 102721 to 130110 — no 103xxx id at all — so a real
        /// current-content boss like 103108 resolved a `boss_monster_id` but
        /// `is_boss` came back false, and `encounter_title`
        /// (`crates/app/src/ui.rs`) rendered an empty header mid-fight. This
        /// covers the same end-to-end path with a boss id now sourced from
        /// `MonsterTable.json`'s `MonsterType == 2` instead of the stale
        /// hand-curated list.
        #[test]
        fn boss_name_resolves_for_an_issue_112_boss_id() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103_108), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103_108));
            assert_eq!(
                snap.encounter.boss_name,
                Some("Paradox-Calamity Remnant - Origin")
            );
            assert!(snap.encounter.is_boss);
        }

        /// issue #76: a meter started mid-pull never sees the boss's
        /// `SyncNearEntities` appear packet, so it only ever receives HP
        /// *deltas* — which carry `AttrHp` and `AttrId` but no `AttrMaxHp`.
        /// Requiring `max_hp` before a boss could resolve left the header
        /// reading "No target" for the entire fight even though the boss's
        /// identity was on the wire the whole time.
        #[test]
        fn boss_resolves_from_curr_hp_alone_when_max_hp_was_never_seen() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 10,
                curr_hp: Some(5_000_000),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: 0,
            }));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert_eq!(snap.encounter.boss_name, Some("Rathalos"));
            assert!(snap.encounter.is_boss);
        }

        /// `max_hp` stays the real boss signal: an enemy with a known
        /// `max_hp` outranks a `curr_hp`-only enemy no matter how much
        /// larger that current HP is. Otherwise a trash mob caught
        /// mid-delta would outvote the boss whose full state we actually
        /// have.
        #[test]
        fn known_max_hp_outranks_a_larger_curr_hp_only_enemy() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&boss_hit(11, 0));
            // Real boss: full state known, but a smaller number.
            m.apply(&hp(10, 100, 100, Some(103), 0));
            // Trash caught mid-delta with a huge current HP and no max.
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 11,
                curr_hp: Some(9_000_000),
                max_hp: None,
                monster_id: Some(10_900),
                timestamp_ms: 0,
            }));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
        }

        /// PR #100 review, finding 2: within the `curr_hp`-only tier, raw HP
        /// magnitude alone lets an *undamaged* trash add outrank a real boss
        /// that has already been burned down. A monster id in
        /// `tables::BOSS_MONSTER_IDS` is the stronger signal and wins.
        #[test]
        fn recognized_boss_outranks_a_larger_curr_hp_only_trash_add() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&boss_hit(11, 0));
            // Real boss, damaged down to 2M of a pool we never saw.
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 10,
                curr_hp: Some(2_000_000),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: 0,
            }));
            // Untouched trash add with a bigger raw number, same tier.
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 11,
                curr_hp: Some(3_000_000),
                max_hp: None,
                monster_id: Some(10_900),
                timestamp_ms: 0,
            }));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert!(snap.encounter.is_boss);
        }

        /// The recognized-boss key also beats the `max_hp` tier: a trash mob
        /// whose full state we happen to have must not take the header slot
        /// from a boss we only see through HP deltas.
        #[test]
        fn recognized_boss_outranks_a_trash_mob_with_a_known_max_hp() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&boss_hit(11, 0));
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 10,
                curr_hp: Some(2_000_000),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: 0,
            }));
            m.apply(&hp(11, 9_000_000, 9_000_000, Some(10_900), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
        }

        /// PR #100 review, finding 3: an `AttrMaxHp` that decodes to 0 is not
        /// a known pool of zero. Ranking it in the `max_hp` tier let it beat
        /// a real mid-pull boss outright, since tier is compared before HP.
        #[test]
        fn a_zero_max_hp_is_treated_as_unknown_when_ranking() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&boss_hit(11, 0));
            // Mid-pull boss, no `max_hp` but a real current HP.
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 10,
                curr_hp: Some(5_000_000),
                max_hp: None,
                monster_id: Some(999_999),
                timestamp_ms: 0,
            }));
            // Junk `max_hp` of 0. Neither is in the boss table, so the tiers
            // alone decide.
            m.apply(&hp(11, 1, 0, Some(10_900), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(999_999));
        }

        /// A monster that has taken damage but whose HP never decoded at
        /// all still cannot be the boss — there is nothing to rank it by.
        #[test]
        fn damaged_enemy_with_no_hp_at_all_does_not_become_the_boss() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, None);
        }

        #[test]
        fn unnamed_monster_id_yields_id_without_a_name() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(999_999), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(999_999));
            assert_eq!(snap.encounter.boss_name, None);
            assert!(!snap.encounter.is_boss);
        }

        #[test]
        fn non_boss_monster_id_yields_no_name_even_when_the_id_is_known() {
            // issue #42: 10900 ("Golden Nappo") has a name in the community
            // table but is not in `tables::BOSS_MONSTER_IDS` — a trash pull
            // must not surface a name just because the id happens to be
            // catalogued. `boss_monster_id` still reflects the real target;
            // only the display fields (`boss_name`, `is_boss`) are gated.
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(10_900), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(10_900));
            assert_eq!(snap.encounter.boss_name, None);
            assert!(!snap.encounter.is_boss);
        }

        #[test]
        fn no_boss_yet_yields_no_boss_monster_id_or_name() {
            let m = Meter::new();
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, None);
            assert_eq!(snap.encounter.boss_name, None);
            assert!(!snap.encounter.is_boss);
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

        // -- dungeon final-boss latch (issue #125) --------------------------

        #[test]
        fn scene_boss_name_latches_for_a_genuine_boss_in_a_dungeon_scene() {
            // 1001 ("Tina's Mindrealm") is a dungeon scene; 103 ("Rathalos")
            // is a genuine boss (`is_boss_monster`).
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));
        }

        #[test]
        fn scene_boss_name_does_not_latch_for_a_non_boss_mid_dungeon_mech() {
            // The exact issue #125 case: template 1342 ("Boss - Battle Mech
            // 03") is not a genuine boss (`MonsterType != 2`), so it must
            // never latch as the dungeon's final boss even though it's the
            // selected `boss_uid` target.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(1342), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_boss_name, None);
        }

        #[test]
        fn scene_boss_name_does_not_latch_in_a_non_dungeon_scene() {
            // 8 ("Asterleeds") is an open-world zone, not a dungeon scene:
            // a world boss fought there must not pin its name to the header
            // for every later visit to that zone.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_boss_name, None);
        }

        #[test]
        fn scene_boss_name_survives_an_encounter_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            m.reset(ResetReason::Manual, 1000);
            let snap = m.snapshot(2000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));
        }

        #[test]
        fn scene_boss_name_overwrites_to_converge_on_the_last_boss_engaged() {
            // A dungeon fought through multiple bosses converges on the last
            // one engaged, which is the final boss — overwriting, not
            // first-write-wins, is the intended behavior.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, Some(103), 0));
            assert_eq!(m.snapshot(1000).encounter.scene_boss_name, Some("Rathalos"));

            m.reset(ResetReason::Manual, 1000);
            m.apply(&boss_hit(11, 1000));
            m.apply(&hp(11, 100, 100, Some(103_108), 1000));
            let snap = m.snapshot(2000);
            assert_eq!(
                snap.encounter.scene_boss_name,
                Some("Paradox-Calamity Remnant - Origin")
            );
        }
    }

    /// issue #69: `scene_transition_log`/`boss_transition_log` are the pure
    /// decision functions behind `Meter::apply`'s and `recompute_boss`'s
    /// sparse diagnostics. Tested directly (rather than by capturing actual
    /// `log::info!` output, which this workspace has no harness for) so
    /// "logs on change, silent on repeat" is asserted without needing one.
    mod diagnostics {
        use super::*;

        #[test]
        fn scene_transition_log_fires_only_when_the_id_changes() {
            assert!(scene_transition_log(None, Some(8)).is_some());
            assert!(scene_transition_log(Some(8), Some(8)).is_none());
            assert!(scene_transition_log(Some(8), Some(9)).is_some());
            // Scene clearing (a real transition, e.g. on `ServerChanged`) still logs.
            assert!(scene_transition_log(Some(8), None).is_some());
            // No-op stays silent even when both sides are already empty.
            assert!(scene_transition_log(None, None).is_none());
        }

        #[test]
        fn scene_transition_log_reports_the_resolved_name_or_says_it_did_not_resolve() {
            let msg = scene_transition_log(None, Some(8)).unwrap();
            assert!(msg.contains("id=8"));
            assert!(msg.contains("Asterleeds"));

            let msg = scene_transition_log(None, Some(999_999)).unwrap();
            assert!(msg.contains("id=999999"));
            assert!(msg.contains("<unresolved>"));
        }

        #[test]
        fn scene_transition_log_reports_a_clear() {
            let msg = scene_transition_log(Some(8), None).unwrap();
            assert!(msg.contains("cleared"));
        }

        #[test]
        fn boss_transition_log_fires_only_when_the_uid_changes() {
            assert!(boss_transition_log(None, Some(10), Some(103)).is_some());
            assert!(boss_transition_log(Some(10), Some(10), Some(103)).is_none());
            assert!(boss_transition_log(Some(10), Some(11), Some(103)).is_some());
            // Boss target clearing (a real transition) still logs.
            assert!(boss_transition_log(Some(10), None, None).is_some());
            // No-op stays silent even when both sides are already empty.
            assert!(boss_transition_log(None, None, None).is_none());
        }

        #[test]
        fn boss_transition_log_reports_recognition_and_the_resolved_name() {
            // Recognized boss id with a catalogued name.
            let msg = boss_transition_log(None, Some(10), Some(103)).unwrap();
            assert!(msg.contains("monster_id=103"));
            assert!(msg.contains("recognized_boss=true"));
            assert!(msg.contains("name=Rathalos"));

            // A monster id outside the boss table: recognized_boss=false,
            // name still resolved if catalogued (boss_monster_id itself is
            // real data regardless of recognition — see `EncounterInfo`'s
            // doc comment).
            let msg = boss_transition_log(None, Some(10), Some(10_900)).unwrap();
            assert!(msg.contains("recognized_boss=false"));

            // Unknown monster id entirely.
            let msg = boss_transition_log(None, Some(10), None).unwrap();
            assert!(msg.contains("uid=10"));
            assert!(msg.contains("monster_id=<unknown>"));

            // Boss target cleared.
            let msg = boss_transition_log(Some(10), None, None).unwrap();
            assert!(msg.contains("cleared"));
        }

        #[test]
        fn scene_boss_latch_log_fires_only_when_the_monster_id_changes() {
            assert!(scene_boss_latch_log(None, 103, 1001).is_some());
            assert!(scene_boss_latch_log(Some(103), 103, 1001).is_none());
            assert!(scene_boss_latch_log(Some(103), 103_108, 1001).is_some());
        }

        #[test]
        fn scene_boss_latch_log_reports_the_scene_and_resolved_name() {
            let msg = scene_boss_latch_log(None, 103, 1001).unwrap();
            assert!(msg.contains("scene=1001"));
            assert!(msg.contains("monster_id=103"));
            assert!(msg.contains("name=Rathalos"));

            let msg = scene_boss_latch_log(None, 999_999, 1001).unwrap();
            assert!(msg.contains("<unresolved>"));
        }

        #[test]
        fn preload_summary_log_only_fires_for_a_dungeon_scene() {
            assert!(preload_summary_log(Some(40001), 3, 1).is_some());
            assert!(preload_summary_log(Some(8), 3, 1).is_none()); // town, not a dungeon
            assert!(preload_summary_log(None, 3, 1).is_none()); // no scene known
        }

        #[test]
        fn preload_summary_log_reports_preloaded_active_and_pruned_counts() {
            let msg = preload_summary_log(Some(40001), 5, 2).unwrap();
            assert!(msg.contains("scene=40001"));
            assert!(msg.contains("preloaded=5"));
            assert!(msg.contains("active=3"));
            assert!(msg.contains("pruned=2"));
        }

        #[test]
        fn preload_summary_log_never_leaks_a_name_or_uid() {
            let msg = preload_summary_log(Some(40001), 5, 2).unwrap();
            assert!(!msg.contains("uid"));
            assert!(!msg.contains("name"));
        }
    }
}
