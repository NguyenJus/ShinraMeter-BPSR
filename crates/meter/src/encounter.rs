//! Encounter state machine: routes protocol events into per-player stats and
//! produces the UI-facing `Snapshot` (plan §T2.1/T2.2).

use std::collections::HashMap;

use crate::event::{Class, DamageEvent, EnemyHp, EntityKind, PlayerInfo, ProtocolEvent};
use crate::fight::{FightConfig, FightEndCause, FightState};
use crate::phase;
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
    /// one. Cross-session persistence (issue #131) lives entirely in the app
    /// crate — this crate stays free of disk I/O and only exposes seed/export
    /// accessors (`with_scene_bosses`, `set_scene_bosses`,
    /// `scene_bosses_for_save`) for the app crate to drive.
    scene_bosses: HashMap<u32, u32>,
    /// When the current fight ended, if it has (issue #78). `Some(t)` puts
    /// the meter in [`FightState::Ended`]: the snapshot is rendered as of
    /// `t` rather than the caller's `now_ms`, so rows, totals and the
    /// elapsed timer all hold still until the next fight (or a manual reset
    /// / server change) clears them. Latched by an explicit end signal (a
    /// boss death) or by [`Meter::tick`] once the idle timeout has elapsed;
    /// cleared by `reset`.
    fight_end_ms: Option<u64>,
    /// The monster id whose death latched `fight_end_ms`, if that is what
    /// ended the fight (issue #124). This is what arms phase resumption: a
    /// dungeon's final boss can move through several phases, each a distinct
    /// monster id whose predecessor really dies, and the first hit on the
    /// next phase must resume the held fight rather than reset it.
    ///
    /// Only ever set by [`Meter::end_fight_on_boss_death`], so an
    /// idle-timeout end leaves it `None` and can never resume — walking away
    /// from a pull and coming back to a same-family boss starts a new fight,
    /// which is what the user means by it. Cleared by `reset` (and so by the
    /// `ServerChanged` path, which resets first).
    fight_end_boss_id: Option<u32>,
    /// Whether the fight was ended by a **party wipe** and the attempt is
    /// being held for review (issue #154). A wipe is the end of a pull, not
    /// a reset: the rows freeze exactly as they do on a boss kill, and this
    /// flag is what makes the hold ignore *everything* until the party is
    /// truly re-engaged — the boss's bar refilling, its swings at the
    /// corpses, an AoE tick clipping an add on the run-back. Only a player
    /// damaging a recognized boss again lifts it, through the ordinary
    /// `NewFight` path (see `withholds_after_wipe`).
    ///
    /// Cleared by `reset` — so by that same `NewFight` — and by a server
    /// change, which invalidates the entity state the re-engagement test
    /// reads and hands the hold back to issue #78's ordinary rule.
    wipe_hold: bool,
    /// How many distinct enemies have been seen to die since the last reset
    /// (issue #124). Hands out `EnemyState::death_order` ranks, which
    /// `recompute_boss` uses to keep the most recently killed boss on the
    /// header once a phased fight's phases are all dead.
    deaths_seen: u64,
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
            fight_end_boss_id: None,
            wipe_hold: false,
            deaths_seen: 0,
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

    /// Seeds the learned scene -> final-boss map from a previously-persisted
    /// value (issue #131), so a dungeon whose final boss was learned in an
    /// earlier session already names it on entry this session, rather than
    /// only after the first observed engagement falls back to naming it
    /// mid-fight. Live observation (`recompute_boss`) always takes
    /// precedence going forward: it overwrites a seeded entry the next time
    /// that scene's real boss is engaged, the same way it converges on the
    /// last boss engaged within one session (see `scene_bosses`' doc
    /// comment).
    pub fn with_scene_bosses(scene_bosses: HashMap<u32, u32>) -> Self {
        Self {
            scene_bosses,
            ..Self::new()
        }
    }

    /// Overwrites the learned scene -> final-boss map in place, for callers
    /// (namely `Pipeline`, issue #131) that need to seed it on a `Meter`
    /// already constructed for another reason (e.g. via `with_names_cache`)
    /// rather than at construction time.
    pub fn set_scene_bosses(&mut self, scene_bosses: HashMap<u32, u32>) {
        self.scene_bosses = scene_bosses;
    }

    /// Exports the learned scene -> final-boss map for persistence (issue
    /// #131), mirroring `names_for_save`. Unlike `names_for_save` this
    /// carries no eviction cap or recency order to preserve — the caller
    /// (`scene_bosses_cache` in the app crate) just needs the full map.
    pub fn scene_bosses_for_save(&self) -> HashMap<u32, u32> {
        self.scene_bosses.clone()
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
    /// * the idle timeout: no player damage for `idle_timeout_ms`. That one
    ///   is derived from `last_event_ms` on every call rather than requiring
    ///   a `tick`, so a caller that only ever calls `snapshot` still gets
    ///   the hold — `tick` merely pins it. Suppressed while
    ///   [`Self::engaged_boss_still_up`] (issue #151): a lull is not the end
    ///   of a pull the party is still standing in.
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
        if idle > 0
            && now_ms.saturating_sub(self.last_event_ms) >= idle
            && !self.engaged_boss_still_up()
        {
            Some(self.last_event_ms)
        } else {
            None
        }
    }

    /// Whether the party is mid-pull on a boss that simply is not being hit
    /// right now (issue #151): still inside an instance
    /// (`tables::is_dungeon_scene`), with a recognized boss
    /// (`tables::is_boss_monster`) that has taken damage this fight and is
    /// not known to be dead.
    ///
    /// This is what stops the idle timeout from standing in for an
    /// encounter boundary it cannot represent. A raid's designed immunity
    /// and mechanic windows exceed `idle_timeout_ms` by design, and ending
    /// the fight in one of them freezes the meter mid-pull and then wipes
    /// every row when the party resumes (the `NewFight` reset, which is
    /// only reachable from an already-ended fight). Raising the timeout
    /// would only move the guess; this uses state the meter already has and
    /// covers every cause of a lull — immunity, untargetable, retreat,
    /// add-clear — rather than just phase changes.
    ///
    /// A pull held open this way still ends: the boss dying latches it
    /// (`end_fight_on_boss_death`), a party wipe latches it (issue #154),
    /// leaving the instance clears `scene_id` and hands the fight straight
    /// back to the idle timeout — which, being derived rather than stored,
    /// then ends it retroactively at the last hit.
    ///
    /// Deliberately scoped by `took_damage`, like `has_other_living_boss`:
    /// a boss standing in the room the party walked past is not a pull in
    /// progress.
    fn engaged_boss_still_up(&self) -> bool {
        self.in_dungeon_scene()
            && self.enemies.values().any(|e| {
                e.took_damage && e.is_alive() && e.monster_id.is_some_and(tables::is_boss_monster)
            })
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
            self.latch_fight_end(FightEndCause::IdleTimeout, end_ms);
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
                // issue #138: a server change (reconnect/zone transition)
                // only invalidates state keyed on identifiers that are
                // valid within one server session — uids are re-issued by
                // the new server, and the scene id is unknown until the
                // next `EnterScene`. It deliberately does **not** clear
                // `players`/totals: those are display state, and a
                // reconnect does not make them wrong. The next real fight's
                // `NewFight` reset (`apply_damage`, below) is what clears
                // them, exactly as it does after an idle-timeout hold.
                //
                // issue #12: a server change is as real a scene change as
                // any (the old scene's preloads can't possibly still be in
                // AOI range afterward), so mirror the Scene arm above and
                // drop preloaded rows nobody ever damaged — logging the
                // same summary line — while `scene_id` still names the
                // scene being left. Rows with real activity survive: they
                // are the display state this arm deliberately keeps.
                self.prune_stale_preloads();
                self.enemies.clear();
                self.boss_uid = None;
                if let Some(msg) = scene_transition_log(self.scene_id, None) {
                    log::info!("{msg}");
                }
                self.scene_id = None;

                // Freeze the fight clock across the zoning gap, same as the
                // idle timeout does, so the held elapsed timer does not run
                // while the connection is down — and so `fight_end_ms`
                // being `Some` arms the `NewFight` path for the
                // reconnecting player's first real hit. A fight already
                // held (or none running at all) is left exactly as-is.
                if self.fight_start_ms.is_some() && self.fight_end_ms.is_none() {
                    self.latch_fight_end(FightEndCause::ServerChanged, *timestamp_ms);
                }

                // issue #154: the wipe hold's re-engagement test reads the
                // enemy map that was just cleared, so it can no longer
                // recognize anything. Leaving the instance hands the hold
                // back to issue #78's ordinary rule, where the next real
                // hit clears it.
                self.wipe_hold = false;

                None
            }
        }
    }

    fn apply_damage(&mut self, d: &DamageEvent) -> Option<ResetReason> {
        // issue #78: pin the end *before* this event touches the encounter's
        // clocks — a monster's swing at a player extends `last_event_ms`
        // without ever producing a row, which would otherwise drag an
        // already-ended fight back into `Active`.
        if let Some(end_ms) = self.fight_ended_at(d.timestamp_ms) {
            self.latch_fight_end(FightEndCause::IdleTimeout, end_ms);
        }

        // issue #124: before the hold is allowed to clear the board, check
        // whether this hit is the *same fight continuing* rather than a new
        // one. A dungeon's final boss can run through several phases, each a
        // distinct monster id whose predecessor genuinely dies and latches
        // the end here; resuming keeps `fight_start_ms` and every
        // accumulated row so the encounter reads as the single fight it was.
        //
        // Placed between the pin above and the `NewFight` reset below on
        // purpose. It has to run after the pin, because the phase gap can
        // easily outlast `idle_timeout_ms` and it is that pin which puts
        // `fight_end_ms` in the state this branch reads. It has to run
        // before the reset, because the reset is exactly what it exists to
        // prevent. And it reads the target's `monster_id` out of `self
        // .enemies`, which is safe this early: `DamageEvent` carries no
        // monster id at all and the `took_damage` bookkeeping further down
        // never writes one either — the only source is a prior
        // `ProtocolEvent::EnemyHp`, so looking it up here sees exactly what
        // looking it up after the bookkeeping would.
        if self.resumes_held_fight(d) {
            self.fight_end_ms = None;
            self.fight_end_boss_id = None;
        }

        // Real combat activity — a player landing a hit — is the *only*
        // thing that ends the hold, and it does so through the existing
        // reset machinery, so this event lands in a clean encounter. Gated
        // on the same condition that starts the fight clock below: a monster
        // swinging at a player in town, or a heal, must not wipe the numbers
        // the user is looking at.
        //
        // ...plus `withholds_new_fight`, which is the narrow exception the
        // phase-resume window carves out of that rule (issue #124): while a
        // phase change is pending, a hit that is *not* positive evidence of
        // a different fight decides nothing.
        let mut reason = None;
        if self.fight_end_ms.is_some()
            && d.attacker_kind == EntityKind::Player
            && !d.is_heal
            && !self.withholds_new_fight(d)
            && !self.withholds_after_wipe(d)
        {
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
            // issue #154: that death may have been the last one standing.
            // A wipe is a fight *end* — the moment a damage meter is most
            // useful — so latch the hold here instead of leaving the
            // attempt to be destroyed by the HP-rollback heuristic when the
            // boss's bar refills a second later.
            if self.party_is_wiped() {
                self.latch_fight_end(FightEndCause::Wipe, d.timestamp_ms);
                self.wipe_hold = true;
            }
        }

        // Healing view is a non-goal: heal events never touch damage totals
        // or fight timing.
        if d.is_heal {
            return reason;
        }

        if d.target_kind == EntityKind::Monster {
            self.enemies.entry(d.target_uid).or_default().took_damage = true;
            // issue #124: remember that this one died, and in what order, so
            // the "is any other boss in this encounter still alive?" question
            // below has an answer even when no HP sync ever reports the
            // corpse at 0 — and so `recompute_boss` can keep the header on
            // the phase that just fell. Must run before `recompute_boss`,
            // which reads both.
            if d.is_dead {
                self.mark_enemy_dead(d.target_uid);
            }
            self.recompute_boss();
            // issue #78: a recognized boss dying ends the fight now, rather
            // than after the idle timeout, so the meter freezes on the kill
            // instead of on a straggler's last tick of DoT damage.
            if self.fight_cfg.end_on_boss_death && d.is_dead && self.boss_uid == Some(d.target_uid)
            {
                self.end_fight_on_boss_death(d.target_uid, d.timestamp_ms);
            }
        }

        // Only player attackers start the fight clock and produce rows;
        // monster damage is tracked above for boss-selection/reset purposes
        // only. Starting the clock on monster damage would let a boss
        // attacking the tank before players open fire dilute every row's DPS
        // with idle time.
        if d.attacker_kind != EntityKind::Player {
            return reason;
        }

        // issue #155: below the early return, not above it. `last_event_ms`
        // is read by exactly two things — the idle-timeout half of
        // `fight_ended_at` and the DPS window in `snapshot` — and both mean
        // "player combat activity". Advancing it on monster damage let a
        // boss swinging at the party's corpses after a wipe push the idle
        // deadline out forever: the fight never ended, the elapsed timer ran
        // on, and every row's DPS decayed as dead time was divided into it —
        // the exact dilution the early return above was written to prevent.
        // Nothing needs monster-activity timing, so there is no second field
        // to track it in.
        self.last_event_ms = self.last_event_ms.max(d.timestamp_ms);

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
    ///
    /// issue #124: the latch is additionally suppressed while the encounter
    /// still holds another *living, damaged, recognized* boss. In a genuine
    /// multi-phase fight the phases are distinct `MonsterType == 2` ids and
    /// an earlier one can carry the larger `max_hp` — so `recompute_boss`
    /// selects it, and without this guard its death would freeze the meter
    /// mid-encounter while the party fights the phase that is still up. The
    /// same guard covers a multi-part boss (`Dragonbane Golem`'s two
    /// cannons) and a raid boss pulled alongside another. Suppressing costs
    /// only the instant freeze — the idle timeout still ends the fight.
    fn end_fight_on_boss_death(&mut self, uid: i64, now_ms: u64) {
        let monster_id = self.enemies.get(&uid).and_then(|e| e.monster_id);
        let recognized = monster_id.is_some_and(tables::is_boss_monster);
        // Guarded on an in-progress fight so a kill packet arriving while no
        // fight is running (the tail of a pull the user just reset away)
        // can't leave a stale end time latched for the *next* fight to trip
        // over.
        if recognized
            && self.fight_start_ms.is_some()
            && self.fight_end_ms.is_none()
            && !self.has_other_living_boss(uid)
        {
            self.latch_fight_end(FightEndCause::BossDeath, now_ms);
            self.fight_end_boss_id = monster_id;
        }
    }

    /// Latches the fight end at `end_ms` and logs the single `info`-level
    /// line that says a fight ended and why (issue #151's diagnostics gap).
    ///
    /// Every path that ends a fight goes through here — boss death, idle
    /// timeout, party wipe, server change — so the line fires exactly once
    /// per fight end: a fight already latched returns untouched, which is
    /// also what makes the repeated "pin the end" calls in `apply_damage`
    /// and `tick` idempotent.
    fn latch_fight_end(&mut self, cause: FightEndCause, end_ms: u64) {
        if self.fight_end_ms.is_some() {
            return;
        }
        self.fight_end_ms = Some(end_ms);
        log::info!("{}", fight_end_log(cause, self.boss_monster_id()));
    }

    /// The monster id of the currently selected boss target, if it has one.
    fn boss_monster_id(&self) -> Option<u32> {
        self.boss_uid
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.monster_id)
    }

    /// Records that `uid` has died, assigning it the next rank in this
    /// encounter's death order (issue #124). Idempotent: the first signal
    /// wins, so a death packet followed by the corpse's HP sync to 0 (or a
    /// retransmit of either) does not re-stamp the rank and reshuffle
    /// `recompute_boss`'s view of who fell last.
    fn mark_enemy_dead(&mut self, uid: i64) {
        let next = self.deaths_seen + 1;
        let assigned = match self.enemies.get_mut(&uid) {
            Some(enemy) if enemy.death_order.is_none() => {
                enemy.death_order = Some(next);
                true
            }
            _ => false,
        };
        if assigned {
            self.deaths_seen = next;
        }
    }

    /// Whether some enemy other than `dying_uid` is a recognized boss that
    /// has taken damage this fight and is not known to be dead (issue #124).
    ///
    /// `took_damage` is what scopes this to the current encounter: siblings
    /// that spawned in the same room-load batch but were never engaged (the
    /// 89.8M-max-HP neighbour in issue #124's capture) are invisible here,
    /// exactly as they are to `recompute_boss`. "Not known to be dead" is
    /// [`EnemyState::is_alive`], which counts an enemy whose HP was never
    /// observed as alive — see its doc comment for why that asymmetry is the
    /// safe one.
    ///
    /// Mostly a backstop, since `recompute_boss` now ranks a living
    /// recognized boss above a dead one and so usually moves `boss_uid` off
    /// the corpse before this is reached at all. What it still catches is the
    /// enemy `recompute_boss` cannot rank: one with neither `max_hp` nor
    /// `curr_hp` is filtered out of the ranking entirely, so a living damaged
    /// boss known only by its `monster_id` would otherwise be invisible and
    /// the dead phase's latch would fire over the top of it.
    fn has_other_living_boss(&self, dying_uid: i64) -> bool {
        self.enemies.iter().any(|(uid, e)| {
            *uid != dying_uid
                && e.took_damage
                && e.is_alive()
                && e.monster_id.is_some_and(tables::is_boss_monster)
        })
    }

    /// Whether every party member the meter knows about is down (issue
    /// #154), i.e. the fight in progress is over and lost.
    ///
    /// The roster is `players`: every uid the meter has seen act, plus the
    /// party members preloaded from the game's own roster packet in an
    /// instance (issue #12/#145/#149). `deaths` is per-encounter — `reset`
    /// drops the rows that carry it — so "has died" means "has died in this
    /// attempt", which is exactly the question. An empty roster is never a
    /// wipe, and neither is a death outside a running fight.
    ///
    /// Detecting the wipe directly is what retires the HP-rollback
    /// heuristic for this case: the rollback shape depends on how fast a
    /// particular boss's bar refills relative to the 9s idle timeout, which
    /// is why the same wipe used to go either way.
    fn party_is_wiped(&self) -> bool {
        self.fight_start_ms.is_some()
            && self.fight_end_ms.is_none()
            && !self.players.is_empty()
            && self.players.values().all(|p| p.deaths > 0)
    }

    /// Whether the wipe hold forbids reading `d` as the first hit of the
    /// next fight (issue #154).
    ///
    /// Re-engagement means a player damaging a *recognized* boss again —
    /// nothing else. The run-back through an instance is full of player
    /// damage that is not a new pull (AoE clipping adds, DoTs finishing off
    /// trash), and clearing the attempt on any of it is the very thing the
    /// hold exists to prevent. A target whose `monster_id` has not arrived
    /// yet is undecidable, so it withholds too — packet order is not
    /// guaranteed and the next hit decides once the `EnemyHp` lands.
    fn withholds_after_wipe(&self, d: &DamageEvent) -> bool {
        self.wipe_hold
            && !self
                .target_monster_id(d)
                .is_some_and(tables::is_boss_monster)
    }

    /// Whether `d` is the next phase of the fight currently being held, and
    /// so should resume it instead of clearing it (issue #124).
    ///
    /// Every condition is load-bearing:
    ///
    /// * a fight is being held, and it was ended by a *boss death* — an
    ///   idle-timeout end leaves `fight_end_boss_id` `None` and never
    ///   resumes;
    /// * a player is landing a real (non-heal) hit on a monster — the same
    ///   gate the `NewFight` reset uses, so a monster swinging at the party
    ///   in town cannot resume anything;
    /// * the target is a recognized boss in the same curated phase group as
    ///   the boss whose death ended the fight (see [`crate::phase`]). A raid's
    ///   three sequential bosses are in different groups (or none), so they
    ///   still take the `NewFight` path — that is the whole distinction this
    ///   function draws;
    /// * the hit lands within `FightConfig::phase_resume_window_ms` of the
    ///   end, so re-entering the same dungeon much later starts a fresh fight
    ///   rather than resuming a stale one.
    ///
    /// A *missed* swing resumes like any other: `is_miss` is deliberately not
    /// consulted here or in the `NewFight` gate. A miss is still the party
    /// engaging the next phase — the only thing on the wire that says so, if
    /// the first attacks whiff — and it is treated identically outside a
    /// phase change, where it counts a hit and no damage.
    fn resumes_held_fight(&self, d: &DamageEvent) -> bool {
        let Some(ended_by) = self.armed_phase_hold(d) else {
            return false;
        };
        self.target_monster_id(d)
            .is_some_and(|id| tables::is_boss_monster(id) && phase::same_phase_group(ended_by, id))
    }

    /// Whether the armed phase-resume window forbids reading `d` as the first
    /// hit of a *new* fight (issue #124, PR #144 review).
    ///
    /// [`Self::resumes_held_fight`] has already run and did not clear the
    /// hold, so `d` is not the next phase. That leaves three shapes, and only
    /// one of them is evidence of anything:
    ///
    /// * the target is a **recognized boss** in another (or no) phase group —
    ///   a genuinely different pull, so the `NewFight` reset stands;
    /// * the target is a **known non-boss**: a straggling add, or a player
    ///   AoE/DoT tick landing on trash while the party waits out the
    ///   transition cutscene. Resetting on that is issue #124's own symptom
    ///   reproduced inside the window built to prevent it — it wipes the
    ///   dead phase's rows and restarts the clock;
    /// * the target's `monster_id` is **not known yet**. Packet order is not
    ///   guaranteed, so the first swing at the next phase can decode before
    ///   the `EnemyHp` that names it. Undecidable is not "new fight": clearing
    ///   here would also drop `fight_end_boss_id`, so the resume could never
    ///   be retried once the id arrived.
    ///
    /// Withholding only defers — it never extends the hold. The window's own
    /// expiry ends it, after which every player hit clears the fight exactly
    /// as issue #78 specifies. That contract is also why this is gated on
    /// [`phase::has_phase_group`]: a fight ended by a boss with no next phase
    /// can never be resumed, so it must not soften the rule either.
    fn withholds_new_fight(&self, d: &DamageEvent) -> bool {
        self.armed_phase_hold(d).is_some()
            && !self
                .target_monster_id(d)
                .is_some_and(tables::is_boss_monster)
    }

    /// The monster id whose death ended the held fight, if that hold is
    /// *armed for a phase change* and `d` could be part of one: a curated
    /// multi-phase boss (see [`crate::phase`]) and a player's real, non-heal
    /// hit landing within `FightConfig::phase_resume_window_ms` of the end.
    ///
    /// The shared precondition of [`Self::resumes_held_fight`] and
    /// [`Self::withholds_new_fight`], which then ask two different questions
    /// about the same window: is this hit the next phase, and is it too
    /// ambiguous to be called a new fight.
    fn armed_phase_hold(&self, d: &DamageEvent) -> Option<u32> {
        let window = self.fight_cfg.phase_resume_window_ms;
        if window == 0 {
            return None;
        }
        let (Some(end_ms), Some(ended_by)) = (self.fight_end_ms, self.fight_end_boss_id) else {
            return None;
        };
        if !phase::has_phase_group(ended_by) {
            return None;
        }
        if d.attacker_kind != EntityKind::Player || d.is_heal {
            return None;
        }
        if d.timestamp_ms.saturating_sub(end_ms) > window {
            return None;
        }
        Some(ended_by)
    }

    /// The cached `monster_id` of `d`'s target, or `None` when the target is
    /// not a monster or no `EnemyHp` has named it yet. The two callers above
    /// treat those two cases the same way, and both must: a target that is
    /// not a monster is no more a new boss pull than an unidentified one.
    fn target_monster_id(&self, d: &DamageEvent) -> Option<u32> {
        if d.target_kind != EntityKind::Monster {
            return None;
        }
        self.enemies.get(&d.target_uid).and_then(|e| e.monster_id)
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
                // The one signal that un-kills a corpse (PR #144 review,
                // finding 2): HP above zero for an entity that has taken no
                // damage since the last reset — i.e. it is not part of the
                // encounter in progress — is a respawn for the next pull, so
                // its death rank no longer describes it. The `took_damage`
                // gate is what keeps this from also un-killing a corpse
                // *mid-fight*, where a resync upward is an artefact and the
                // death latch must hold (see the `mark_enemy_dead` call
                // below).
                if curr > 0 && !enemy.took_damage {
                    enemy.death_order = None;
                }
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

        // issue #124: an HP sync to 0 is the other death signal (see the
        // `end_fight_on_boss_death` call below), and the one that survives a
        // missed death packet. Latched the same way `apply_damage` latches
        // `is_dead`, so a corpse whose HP later resyncs upward still reads as
        // dead for the rest of this fight. Before `recompute_boss`, which
        // ranks on it.
        if e.curr_hp == Some(0) {
            self.mark_enemy_dead(e.uid);
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
    /// 2. **Alive** (issue #124). Among equally-recognized enemies a living
    ///    one outranks a dead one, so once a phased boss's Origin phase has
    ///    fallen and the party is hitting Continuation, the header follows
    ///    the phase actually being fought — and Continuation's own death
    ///    then latches the fight end through the ordinary
    ///    `boss_uid == target_uid` path instead of falling through to the
    ///    idle timeout. Deliberately *below* `recognized`: a dead recognized
    ///    boss must still outrank a living unrecognized add, or the header
    ///    would flip to a straggling trash mob the instant the boss died,
    ///    which is exactly what issue #78's post-kill hold exists to avoid.
    /// 3. **Death order** among the dead (issue #124): the most recently
    ///    killed wins. This only ever discriminates when everything damaged
    ///    is dead — the ordinary end of a fight — where it keeps the header
    ///    on the boss the party just killed. A phased fight would otherwise
    ///    fall back to `max_hp` here and name the *first* phase, since
    ///    issue #124's premise is that an earlier phase carries the larger
    ///    pool; that would also break the final phase's own death latch.
    ///    Living enemies all share rank 0, so this never perturbs them.
    /// 4. **HP tier**: a known `max_hp` (tier 1) outranks a `curr_hp`-only
    ///    enemy (tier 0), however large that current HP is — `max_hp` is the
    ///    real HP-side boss signal, while current HP is a moving number a
    ///    healthy trash mob can top while the boss sits at 10%. A
    ///    `max_hp` of `Some(0)` is treated as *unknown*, not as a known pool
    ///    of zero: otherwise a wire value that varint-decodes to 0 would win
    ///    tier 1 outright over a real mid-pull boss at 5M. This matches
    ///    `EnemyState::pct`, which already guards on `max > 0`.
    /// 5. **HP magnitude** within a tier, then **uid** to tie-break
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
                let alive = u8::from(e.is_alive());
                // Living enemies all share death rank 0 so the key is inert
                // for them; among the dead it orders by who fell last.
                let died = e.death_order.unwrap_or(0);
                match (e.max_hp.filter(|max| *max > 0), e.curr_hp) {
                    (Some(max), _) => Some((*uid, recognized, alive, died, 1u8, max)),
                    (None, Some(curr)) => Some((*uid, recognized, alive, died, 0u8, curr)),
                    (None, None) => None,
                }
            })
            .max_by_key(|(uid, recognized, alive, died, tier, hp)| {
                (*recognized, *alive, *died, *tier, *hp, *uid)
            })
            .map(|(uid, ..)| uid);

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
        let boss_hp_pct = self
            .boss_uid
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.pct());
        let party_down = self.players.values().filter(|p| p.deaths > 0).count();
        log::info!(
            "{}",
            reset_log(reason, boss_hp_pct, party_down, self.players.len())
        );
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
            // `death_order` deliberately survives (PR #144 review, finding
            // 2). A reset is bookkeeping, not a resurrection: it says nothing
            // about whether the corpse is back on its feet, and the rest of
            // `EnemyState` — `curr_hp` included — survives for the same
            // reason. Clearing it here made `EnemyState::is_alive` fall back
            // to a stale `curr_hp`, so a boss killed by a death packet whose
            // last HP sync was above zero read as *living* for the whole next
            // fight, blocking the next boss's end latch and outranking it in
            // `recompute_boss`. `apply_enemy_hp` clears the rank instead,
            // when a sync above zero shows the entity actually respawned.
        }
        self.fight_start_ms = None;
        // Every reset reason (manual, boss-HP rollback, server change, and
        // the next fight's first hit) drops the post-fight hold: the numbers
        // being held belong to the encounter that is being cleared.
        self.fight_end_ms = None;
        // ...and with it the phase-resume arming (issue #124): the fight
        // whose boss died is gone, so nothing can be a continuation of it.
        self.fight_end_boss_id = None;
        // ...and the wipe hold (issue #154): the attempt it was protecting
        // is what just got cleared.
        self.wipe_hold = false;
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
        // stay the raw facts about the currently-selected target. Issue
        // #131 inverted which one `encounter_title` (`crates/app/src/ui.rs`)
        // prefers: a genuinely recognized live boss (`is_boss`) now wins
        // over this field, which is the fallback for "nothing engaged yet"
        // and for a non-boss `boss_uid` target — see that function's doc
        // comment for the full precedence and why.
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

/// Builds the "fight ended" diagnostic line (issue #151's diagnostics gap).
/// Unlike the transition-only builders above this always returns a line —
/// its only caller, [`Meter::latch_fight_end`], already fires exactly once
/// per fight end. Carries the boss's monster id and catalogued name only:
/// never a player name or uid, since these logs get shared for debugging
/// (`crates/app/src/logging.rs`). Pure, like the builders around it, for
/// the same testability reason.
fn fight_end_log(cause: FightEndCause, boss_monster_id: Option<u32>) -> String {
    let cause = cause.label();
    match boss_monster_id {
        Some(id) => {
            let name = tables::monster_name(id).unwrap_or("<unresolved>");
            format!("encounter: fight ended cause={cause} boss_monster_id={id} name={name}")
        }
        None => format!("encounter: fight ended cause={cause} boss_monster_id=<unknown>"),
    }
}

/// Builds the `reset` diagnostic line. The boss HP percentage and the
/// party down count are what make a `BossHpRollback` and a genuine wipe
/// distinguishable in a log (issue #151's diagnostics gap, issue #154):
/// the rollback shape alone reads the same either way. Counts only — never
/// a player name or uid.
fn reset_log(
    reason: ResetReason,
    boss_hp_pct: Option<f64>,
    party_down: usize,
    party_known: usize,
) -> String {
    let hp = match boss_hp_pct {
        Some(pct) => format!("{pct:.1}"),
        None => "<unknown>".to_string(),
    };
    format!(
        "encounter: reset reason={reason:?} boss_hp_pct={hp} party_down={party_down}/{party_known}"
    )
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

    /// issue #12: `ServerChanged` prunes preloads like any other real scene
    /// change, so `preload_count` can't go stale (and the summary log can't
    /// be skipped) the way an un-pruned `BossHpRollback` would leave it.
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
        // Bob was only ever preloaded, so pruning drops him; Alice landed a
        // real hit, and issue #138 keeps that display state across a
        // reconnect rather than resetting it here.
        let rows = m.snapshot(2000).rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Alice");
    }

    /// The per-scene preload cap guards against a misclassified dungeon
    /// scene preloading unboundedly.
    /// Preloads well past `MAX_PRELOADED_PLAYERS` and asserts both the
    /// counter and the roster stop growing at the cap, which sits
    /// comfortably above the largest real raid this meter supports (20
    /// players — see `preloading_a_full_20_player_raid_snapshots_cleanly`).
    #[test]
    fn preload_count_is_capped_per_scene() {
        const {
            assert!(
                MAX_PRELOADED_PLAYERS > 20,
                "cap must comfortably exceed the largest real raid"
            )
        };
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

    /// Issue #131: the meter-side half of cross-session scene -> final-boss
    /// persistence. `scene_bosses_cache` (app crate) owns the disk I/O; this
    /// only covers the seed-in/export-out contract these tests exercise
    /// directly against `Meter`, mirroring `mod names_cache` above.
    mod scene_bosses {
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

        fn hp(uid: i64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        #[test]
        fn seeded_scene_boss_resolves_before_any_hit_lands_this_session() {
            // 1001 ("Tina's Mindrealm") is a dungeon scene; 103 ("Rathalos")
            // is a genuine boss — seeding mirrors what a real run of this
            // dungeon would have latched last session.
            let mut m = Meter::with_scene_bosses(HashMap::from([(1001, 103)]));
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });

            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));
        }

        #[test]
        fn set_scene_bosses_seeds_a_meter_already_constructed_another_way() {
            let cache = vec![(5, (Some("Cached".to_string()), None))];
            let mut m = Meter::with_names_cache(cache);
            m.set_scene_bosses(HashMap::from([(1001, 103)]));
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });

            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));
        }

        #[test]
        fn scene_bosses_for_save_returns_what_was_learned() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 103, 0));

            assert_eq!(m.scene_bosses_for_save(), HashMap::from([(1001, 103)]));
        }

        #[test]
        fn scene_bosses_for_save_is_empty_when_nothing_has_been_learned() {
            let m = Meter::new();
            assert_eq!(m.scene_bosses_for_save(), HashMap::new());
        }

        #[test]
        fn a_reset_does_not_lose_a_seeded_scene_boss() {
            let mut m = Meter::with_scene_bosses(HashMap::from([(1001, 103)]));
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.reset(ResetReason::Manual, 1000);

            let snap = m.snapshot(2000);
            assert_eq!(snap.encounter.scene_boss_name, Some("Rathalos"));
            assert_eq!(m.scene_bosses_for_save(), HashMap::from([(1001, 103)]));
        }

        #[test]
        fn live_observation_overwrites_a_seeded_entry() {
            // A game patch (or just a different pull) can make the live
            // final boss diverge from what was seeded — the freshly observed
            // one must win, matching the within-session overwrite semantics
            // `recompute_boss` already documents.
            let mut m = Meter::with_scene_bosses(HashMap::from([(1001, 103)]));
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(11, 0));
            m.apply(&hp(11, 103_108, 0));

            assert_eq!(m.scene_bosses_for_save(), HashMap::from([(1001, 103_108)]));
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
        fn rollback_cooldown_anchors_on_the_reconnect_new_fight_reset() {
            let mut m = Meter::new();
            // Old fight, long since idle.
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));

            // Server change detected 5 minutes later. This no longer resets
            // (issue #138) -- it only latches `fight_end_ms`, so it does
            // *not* anchor the cooldown below.
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 300_000,
            });

            // New zone: boss picked up again well after the reconnect
            // signal itself. This hit is what actually anchors the
            // cooldown -- it fires the `NewFight` reset
            // (`last_reset_ms = 300_800`) that clears the held fight, not
            // the `ServerChanged` moment above.
            m.apply(&boss_hit(10, 300_800));
            m.apply(&hp(10, 55, 100, 300_850));
            let r = m.apply(&hp(10, 96, 100, 302_400));

            // 302_400 - 300_800 = 1_600ms: still inside the cooldown
            // anchored on the reconnect hit -> suppressed. If the cooldown
            // were (wrongly) anchored on the `ServerChanged` moment instead,
            // 302_400 - 300_000 = 2_400ms would already be past the
            // 2_000ms cooldown and this rollback shape would fire for real.
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

        /// issue #138: a server change invalidates uid-keyed entity state
        /// (uids are re-issued by the new server) but must not touch the
        /// displayed player stats — those are cleared later, by the next
        /// fight's `NewFight` reset, not here.
        #[test]
        fn server_changed_clears_enemies_but_keeps_players() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 0));
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            let r = m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 1000 });
            assert_eq!(r, None, "a server change must not report a reset");
            let snap = m.snapshot(1000);
            assert!(
                !snap.rows.is_empty(),
                "player rows must survive a reconnect"
            );
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

        /// issue #138: a server change (reconnect/zoning) must not wipe the
        /// numbers the player is still reading. It only invalidates
        /// entity/scene state, and — since the fight was already held —
        /// leaves the freeze exactly where it was.
        #[test]
        fn server_change_freezes_but_does_not_clear_an_already_ended_fight() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            assert_eq!(m.tick(100_000), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 100_000,
            });
            assert_eq!(reason, None, "a server change must not report a reset");
            let snap = m.snapshot(200_000);
            assert_eq!(snap.total_damage, 5_000);
            assert!(!snap.rows.is_empty());
            assert_eq!(m.fight_state(200_000), FightState::Ended);
        }

        /// A reconnect mid-fight (still `Active`, not yet held) must latch
        /// `fight_end_ms` to the `ServerChanged` timestamp — freezing the
        /// clock across the zoning gap and arming the `NewFight` path —
        /// while keeping the accumulated stats, and must invalidate the
        /// uid-keyed entity state and the scene id.
        #[test]
        fn server_change_mid_fight_latches_the_clock_and_keeps_the_stats() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 7 });
            m.apply(&dmg(1, 700, 0));
            m.apply(&boss_hit(10, 100, false));
            m.apply(&hp(10, 50, Some(103), 100));
            assert_eq!(m.snapshot(100).encounter.scene_id, Some(7));

            // Well inside the idle window: still active, not yet held.
            assert_eq!(m.fight_state(500), FightState::Active);
            let reason = m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 500 });
            assert_eq!(reason, None, "a server change must not report a reset");

            assert_eq!(m.fight_state(600_000), FightState::Ended);
            let snap = m.snapshot(600_000);
            assert_eq!(
                snap.total_damage, 800,
                "player totals must survive a reconnect"
            );
            assert!(!snap.rows.is_empty());
            assert_eq!(
                snap.duration_ms, 500,
                "the clock latches to the ServerChanged timestamp, not fight_start_ms drifting"
            );
            assert!(m.enemies.is_empty(), "uids are re-issued by the new server");
            assert!(m.boss_uid.is_none());
            assert_eq!(snap.encounter.scene_id, None);
        }

        /// No fight was running at all: a server change must not conjure a
        /// fight end (or anything else) out of nothing.
        #[test]
        fn server_change_while_idle_touches_nothing() {
            let mut m = Meter::new();
            assert_eq!(m.fight_state(1_000), FightState::Idle);
            let reason = m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 1_000,
            });
            assert_eq!(reason, None);
            assert_eq!(m.fight_state(2_000), FightState::Idle);
            let snap = m.snapshot(2_000);
            assert_eq!(snap.total_damage, 0);
            assert!(snap.rows.is_empty());
        }

        /// The reconnecting player's first real hit is what finally clears
        /// the pre-disconnect numbers — the same `NewFight` path an
        /// idle-timeout hold uses, not a new reset kind.
        #[test]
        fn server_change_then_next_fights_first_hit_clears_the_held_stats() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 700, 0));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 500 });
            assert_eq!(m.fight_state(500), FightState::Ended);

            let reason = m.apply(&dmg(1, 300, 10_000));
            assert_eq!(reason, Some(ResetReason::NewFight));
            let snap = m.snapshot(11_000);
            assert_eq!(
                snap.total_damage, 300,
                "the pre-disconnect damage must be gone"
            );
            assert_eq!(m.fight_state(11_000), FightState::Active);
        }

        /// The same character can come back under a different uid after a
        /// reconnect (issue #138's double-count risk). `NewFight`'s
        /// `players.clear()` drops the whole map rather than merging by
        /// uid, so the old uid's row cannot survive into — or be summed
        /// with — the new one.
        #[test]
        fn a_reconnect_uid_change_does_not_double_count_with_the_old_uid() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 700, 0));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 500 });

            // The same player returns under uid 2, not uid 1.
            let reason = m.apply(&dmg(2, 300, 10_000));
            assert_eq!(reason, Some(ResetReason::NewFight));

            let snap = m.snapshot(11_000);
            assert_eq!(
                snap.total_damage, 300,
                "the old uid's damage must not survive into the new fight"
            );
            assert_eq!(snap.rows.len(), 1);
            assert_eq!(snap.rows[0].uid, 2);
        }

        /// Mirrors `a_monster_swinging_at_a_player_does_not_end_the_hold`:
        /// combat the user isn't part of must not end a hold that started
        /// with a server change either.
        #[test]
        fn a_monster_hit_after_a_server_change_does_not_end_the_hold() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 500 });
            assert_eq!(m.fight_state(500), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 99,
                attacker_kind: EntityKind::Monster,
                target_uid: 1,
                target_kind: EntityKind::Player,
                value: 200,
                timestamp_ms: 10_000,
                ..Default::default()
            }));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(11_000).total_damage, 5_000);
            assert_eq!(m.fight_state(11_000), FightState::Ended);
        }

        /// Mirrors `a_heal_does_not_end_the_hold` for the server-change
        /// case.
        #[test]
        fn a_heal_after_a_server_change_does_not_end_the_hold() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 0));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 500 });

            let reason = m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                value: 400,
                is_heal: true,
                timestamp_ms: 10_000,
                ..Default::default()
            }));
            assert_eq!(reason, None);
            assert_eq!(m.snapshot(11_000).total_damage, 5_000);
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

        // -- issue #151: the idle timeout must not end a live pull --------

        /// Any `tables::is_dungeon_scene` id.
        const DUNGEON_SCENE: u32 = 1_001;
        /// "Rathalos", a recognized boss.
        const BOSS: u32 = 103;
        /// "Golden Nappo": named but `MonsterType == 0`, i.e. trash.
        const TRASH: u32 = 10_900;

        fn in_dungeon() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_SCENE,
            });
            m
        }

        #[test]
        fn an_idle_lull_does_not_end_the_fight_while_a_dungeon_boss_is_still_up() {
            // The raid immunity/mechanic window from issue #151: nothing can
            // be hit for far longer than the 9s idle timeout, but the pull is
            // still very much in progress.
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + 10 * idle()), FightState::Active);
            assert_eq!(m.tick(1_000 + 10 * idle()), FightState::Active);
        }

        #[test]
        fn the_idle_timeout_still_ends_a_pull_on_trash_in_a_dungeon() {
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(TRASH), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + idle()), FightState::Ended);
        }

        #[test]
        fn the_idle_timeout_still_ends_a_boss_fight_outside_a_dungeon() {
            // A world boss in an open-world zone: no instance to be stuck
            // in, so the ordinary freeze applies.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + idle()), FightState::Ended);
        }

        #[test]
        fn a_dead_dungeon_boss_does_not_hold_the_fight_open() {
            // The control for the case above: the same boss in the same
            // instance, but dead. The kill still freezes the meter
            // instantly, and nothing holds the fight open afterwards.
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            m.apply(&boss_hit(10, 2_000, true));
            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        #[test]
        fn leaving_the_dungeon_lets_the_idle_timeout_end_a_held_boss_pull() {
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(60_000), FightState::Active);

            // Walking out of the instance: the pull is over.
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
            assert_eq!(m.fight_state(60_000), FightState::Ended);
            assert_eq!(
                m.snapshot(60_000).duration_ms,
                1,
                "the fight still ended at its last hit, not on leaving"
            );
        }

        // -- issue #155: monster damage must not extend the fight ---------

        /// A monster swinging at a player: the shape that keeps arriving
        /// after a wipe, when the boss carries on hitting corpses.
        fn monster_hit(target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 99,
                attacker_kind: EntityKind::Monster,
                target_uid,
                target_kind: EntityKind::Player,
                value: 200,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        #[test]
        fn monster_damage_does_not_hold_the_fight_open_past_the_idle_window() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 5_000, 1_000));
            // The party is down; the boss keeps swinging once a second for
            // far longer than the idle window. None of it is a reason to
            // keep the elapsed timer running.
            for ts in (2_000..=30_000).step_by(1_000) {
                m.apply(&monster_hit(1, ts));
            }
            assert_eq!(m.fight_state(30_500), FightState::Ended);
            assert_eq!(
                m.snapshot(30_500).duration_ms,
                1,
                "the elapsed timer must freeze at the last player damage"
            );
        }

        #[test]
        fn monster_damage_does_not_extend_the_dps_window() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 10_000, 0));
            m.apply(&dmg(1, 10_000, 2_000));
            let before = m.snapshot(2_000).total_dps;
            m.apply(&monster_hit(1, 6_000));
            assert!(
                (m.snapshot(6_000).total_dps - before).abs() < 0.001,
                "a monster's swing must not dilute DPS with idle time"
            );
        }
    }

    /// Issue #154/#155: a party wipe is the *end of a pull*, not a reset.
    /// The attempt's rows freeze for review, and nothing that happens
    /// afterwards — the boss's HP bar refilling, adds swinging at corpses,
    /// a stray AoE tick on trash during the run-back — touches them until
    /// the party genuinely re-engages the boss.
    mod wipe {
        use super::*;

        /// Any `tables::is_dungeon_scene` id: a wipe is an instance thing.
        const RAID_SCENE: u32 = 1_001;
        /// Paradox-Calamity Remnant (Origin), a recognized boss.
        const BOSS: u32 = 103_108;
        /// "Golden Nappo": named but `MonsterType == 0`, i.e. a trash add.
        const TRASH: u32 = 10_900;
        const BOSS_UID: i64 = 10;
        const ADD_UID: i64 = 11;

        fn hit(attacker_uid: i64, target_uid: i64, value: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid,
                attacker_kind: EntityKind::Player,
                target_uid,
                target_kind: EntityKind::Monster,
                value,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        fn enemy_hp(uid: i64, curr: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(1_000_000),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// The boss landing a killing blow on a party member.
        fn killing_blow(target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: BOSS_UID,
                attacker_kind: EntityKind::Monster,
                target_uid,
                target_kind: EntityKind::Player,
                value: 9_999,
                is_dead: true,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// The boss carrying on swinging after the party is down.
        fn monster_swing(target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: BOSS_UID,
                attacker_kind: EntityKind::Monster,
                target_uid,
                target_kind: EntityKind::Player,
                value: 200,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// A two-player party in an instance, both rows known from the
        /// roster (issue #145/#149) and both engaged on the boss, which has
        /// been burned to 20% of its bar.
        fn pull() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: RAID_SCENE,
            });
            m.apply(&player_info(1, "Alpha"));
            m.apply(&player_info(2, "Bravo"));
            m.apply(&enemy_hp(BOSS_UID, 1_000_000, BOSS, 0));
            m.apply(&hit(1, BOSS_UID, 5_000, 1_000));
            m.apply(&hit(2, BOSS_UID, 5_000, 1_500));
            m.apply(&enemy_hp(BOSS_UID, 200_000, BOSS, 4_000));
            m
        }

        /// ...and then everybody dies.
        fn wiped() -> Meter {
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            m.apply(&killing_blow(2, 6_000));
            m
        }

        #[test]
        fn a_full_party_wipe_ends_the_fight_and_freezes_the_rows() {
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            assert_eq!(
                m.fight_state(5_500),
                FightState::Active,
                "one player down is not a wipe"
            );

            m.apply(&killing_blow(2, 6_000));
            assert_eq!(m.fight_state(6_500), FightState::Ended);

            // The attempt is still on screen a minute later, for review.
            let snap = m.snapshot(66_000);
            assert_eq!(snap.total_damage, 10_000);
            assert_eq!(snap.rows.len(), 2);
            assert_eq!(
                snap.duration_ms, 5_000,
                "frozen at the wipe (6_000) minus the first hit (1_000)"
            );
        }

        #[test]
        fn a_roster_member_still_standing_is_not_a_wipe() {
            let mut m = pull();
            // A third party member the roster named but who never attacked
            // and never died: the party is not down.
            m.apply(&player_info(3, "Cypress"));
            m.apply(&killing_blow(1, 5_000));
            m.apply(&killing_blow(2, 6_000));
            assert_eq!(m.fight_state(6_500), FightState::Active);
        }

        #[test]
        fn monster_damage_during_the_wipe_hold_does_not_restart_the_clock() {
            let mut m = wiped();
            for ts in (7_000..=60_000).step_by(1_000) {
                m.apply(&monster_swing(1, ts));
            }
            assert_eq!(m.fight_state(61_000), FightState::Ended);
            assert_eq!(m.snapshot(61_000).duration_ms, 5_000);
            assert_eq!(m.snapshot(61_000).total_damage, 10_000);
        }

        #[test]
        fn the_boss_bar_refilling_after_a_wipe_does_not_reset_the_attempt() {
            let mut m = wiped();
            // The bar snaps back to full a second after the last party
            // member falls — the shape `check_hp_rollback` reads as a wipe,
            // arriving well inside the 9s idle window that used to be the
            // only thing making `held` true.
            let r = m.apply(&enemy_hp(BOSS_UID, 1_000_000, BOSS, 7_000));
            assert_eq!(r, None, "a wipe must freeze the attempt, not clear it");
            assert_eq!(m.snapshot(8_000).total_damage, 10_000);
            assert_eq!(m.fight_state(8_000), FightState::Ended);
        }

        #[test]
        fn hitting_trash_during_the_wipe_hold_does_not_clear_the_held_rows() {
            let mut m = wiped();
            m.apply(&enemy_hp(ADD_UID, 50_000, TRASH, 19_000));
            // Running back in, an AoE clips an add on the way to the boss.
            let r = m.apply(&hit(1, ADD_UID, 900, 20_000));
            assert_eq!(r, None);
            assert_eq!(m.snapshot(21_000).total_damage, 10_000);
            assert_eq!(m.fight_state(21_000), FightState::Ended);
        }

        #[test]
        fn re_engaging_the_boss_after_a_wipe_starts_a_fresh_fight() {
            let mut m = wiped();
            let r = m.apply(&hit(1, BOSS_UID, 400, 30_000));
            assert_eq!(r, Some(ResetReason::NewFight));
            let snap = m.snapshot(31_000);
            assert_eq!(snap.total_damage, 400, "the next pull starts clean");
            assert_eq!(m.fight_state(31_000), FightState::Active);
        }

        #[test]
        fn a_server_change_ends_the_wipe_hold() {
            let mut m = wiped();
            // Leaving the instance: uids are re-issued and no boss is
            // identified on the far side, so the ordinary issue #78 rule
            // takes back over and the next real hit clears the hold.
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 20_000,
            });
            let r = m.apply(&hit(1, 77, 300, 30_000));
            assert_eq!(r, Some(ResetReason::NewFight));
        }
    }

    /// Issue #124: a dungeon's final boss may fight through several phases,
    /// each a distinct `MonsterType == 2` monster id whose predecessor really
    /// dies. Those must not end the fight. A raid's sequential bosses must
    /// still reset it.
    mod multi_phase_boss {
        use super::*;

        /// Dragonbane Golem's cannons (issue #160): all in one curated
        /// phase group, so a stand-in for a three-phase fight without
        /// depending on Paradox-Calamity Remnant, which issue #153 removed
        /// from `BOSS_PHASE_GROUPS` (those ids are three separately
        /// selectable raid bosses, not phases of one fight).
        const ORIGIN: u32 = 103_110;
        const CONTINUATION: u32 = 103_111;
        const FINAL: u32 = 103_301;
        /// "Boss - Crimson Foxen": a recognized boss in no phase group, so a
        /// stand-in for the *next* boss of a raid instance.
        const OTHER_BOSS: u32 = 10_041;
        /// "Golden Nappo": named but `MonsterType == 0`, so a straggling add
        /// that `is_boss_monster` rejects.
        const TRASH: u32 = 10_900;
        /// Any `tables::is_dungeon_scene` id, for the issue #125 latch.
        const DUNGEON_SCENE: u32 = 1_001;

        fn window() -> u64 {
            FightConfig::default().phase_resume_window_ms
        }

        /// A player hit on monster `uid`, optionally the killing blow.
        fn hit(uid: i64, value: i64, ts: u64, is_dead: bool) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value,
                is_dead,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        fn hp(uid: i64, curr: u64, max: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        // -- Part A: don't latch while another phase is still up ------------

        #[test]
        fn an_earlier_phase_dying_does_not_end_the_fight_while_a_later_one_lives() {
            // The exact shape issue #124 describes: the *earlier* phase
            // carries the larger `max_hp`, so `recompute_boss` selects it,
            // and its death used to freeze the meter mid-encounter.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hp(11, 400, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 200, false));
            assert_eq!(m.boss_uid, Some(10), "the larger-max-hp phase is boss");

            m.apply(&hit(10, 100, 300, true));

            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.fight_state(400), FightState::Active);
        }

        #[test]
        fn a_boss_dying_last_still_ends_the_fight_immediately() {
            // The control for the case above, and the issue #78 behaviour
            // that must survive: same two phases, but the other one is
            // already dead when the selected boss falls, so nothing is left
            // to fight and the meter freezes on the kill.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hp(11, 400, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 200, true));
            m.apply(&hit(10, 100, 300, true));

            assert_eq!(m.fight_end_ms, Some(300));
            assert_eq!(m.fight_state(400), FightState::Ended);
        }

        #[test]
        fn an_undamaged_sibling_boss_does_not_block_the_latch() {
            // Issue #124's own capture: siblings spawn in the same room-load
            // batch and are never engaged. `took_damage` scopes the guard to
            // the current encounter, so they stay invisible to it.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(10, 100, 300, true));

            assert_eq!(m.fight_end_ms, Some(300));
        }

        #[test]
        fn a_damaged_boss_with_no_hp_at_all_counts_as_living() {
            // Pins `has_other_living_boss` on its own, without help from the
            // ranking key: an enemy with neither `max_hp` nor `curr_hp` is
            // unrankable, so `recompute_boss` cannot move `boss_uid` off the
            // dying phase and the guard is the only thing standing between
            // this fight and an early end. It also pins the asymmetry
            // documented on `EnemyState::is_alive` — never-observed HP counts
            // as alive, so the fight falls back to the idle timeout, which is
            // always safe.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                uid: 11,
                curr_hp: None,
                max_hp: None,
                monster_id: Some(CONTINUATION),
                timestamp_ms: 0,
            }));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 200, false));
            m.apply(&hit(10, 100, 300, true));

            assert_eq!(m.boss_uid, Some(10), "the other boss is unrankable");
            assert_eq!(m.fight_end_ms, None);
        }

        // -- Part B: resume across a latched end ----------------------------

        #[test]
        fn the_next_phase_resumes_the_held_fight_instead_of_resetting_it() {
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            // Nothing else was damaged, so the kill does latch the end.
            assert_eq!(m.fight_state(1_100), FightState::Ended);
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            // The next phase spawns afterwards, so it had no `took_damage`
            // when the previous one died — Part A cannot see it, and only the
            // phase group can.
            m.apply(&hp(11, 500, 500, CONTINUATION, 20_000));
            let reason = m.apply(&hit(11, 700, 21_000, false));

            assert_eq!(reason, None, "a phase change is not a new fight");
            assert_eq!(m.fight_start_ms, Some(100), "the fight clock keeps running");
            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.fight_end_boss_id, None);
            assert_eq!(m.fight_state(21_100), FightState::Active);
            assert_eq!(
                m.snapshot(21_100).total_damage,
                1_700,
                "damage from before the phase change is still counted"
            );
        }

        #[test]
        fn a_different_boss_in_the_same_instance_still_starts_a_new_fight() {
            // The raid case: three final bosses fought sequentially in one
            // instance must each get their own encounter.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            assert_eq!(m.fight_state(1_100), FightState::Ended);

            m.apply(&hp(11, 500, 500, OTHER_BOSS, 20_000));
            let reason = m.apply(&hit(11, 700, 21_000, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.fight_start_ms, Some(21_000));
            assert_eq!(m.snapshot(21_100).total_damage, 700);
        }

        #[test]
        fn the_same_phase_group_outside_the_grace_window_starts_a_new_fight() {
            // Re-entering the dungeon much later: same boss family, but far
            // too late to be the same pull.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            let late = 1_000 + window() + 1;
            m.apply(&hp(11, 500, 500, CONTINUATION, late));
            let reason = m.apply(&hit(11, 700, late, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(late + 100).total_damage, 700);
        }

        #[test]
        fn the_same_phase_group_at_the_grace_window_edge_still_resumes() {
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            let edge = 1_000 + window();
            m.apply(&hp(11, 500, 500, CONTINUATION, edge));
            let reason = m.apply(&hit(11, 700, edge, false));

            assert_eq!(reason, None);
            assert_eq!(m.fight_start_ms, Some(100));
        }

        #[test]
        fn an_idle_timeout_end_is_never_resumed() {
            // Only a boss *death* arms resumption. Walking away from a pull
            // and coming back to the same boss family is a new fight, which
            // is what the user means by it.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            assert_eq!(m.fight_state(100 + 20_000), FightState::Ended);
            assert_eq!(m.fight_end_boss_id, None);

            m.apply(&hp(11, 500, 500, CONTINUATION, 20_000));
            let reason = m.apply(&hit(11, 700, 20_100, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(20_200).total_damage, 700);
        }

        #[test]
        fn phase_resumption_can_be_disabled() {
            let mut m = Meter::with_fight_config(FightConfig {
                phase_resume_window_ms: 0,
                ..FightConfig::default()
            });
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            m.apply(&hp(11, 500, 500, CONTINUATION, 2_000));
            let reason = m.apply(&hit(11, 700, 2_000, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
        }

        #[test]
        fn a_three_phase_fight_stays_one_encounter_end_to_end() {
            // Deliberately in issue #124's shape: `max_hp` *decreases* across
            // the phases, so on HP alone the first phase would stay selected
            // forever. Each phase is selected in turn anyway, dies, latches
            // the end, and is resumed by the next — proving both that
            // `fight_end_boss_id` re-arms rather than sticking to phase one,
            // and that the header follows the phase being fought.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_SCENE,
            });

            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hit(10, 100, 100, false));
            assert_eq!(m.snapshot(100).encounter.boss_monster_id, Some(ORIGIN));
            m.apply(&hit(10, 100, 1_000, true));
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            m.apply(&hp(11, 1_000, 1_000, CONTINUATION, 5_000));
            m.apply(&hit(11, 100, 5_000, false));
            assert_eq!(
                m.snapshot(5_000).encounter.boss_monster_id,
                Some(CONTINUATION),
                "the header follows the living phase, not the bigger corpse"
            );
            m.apply(&hit(11, 100, 6_000, true));
            assert_eq!(m.fight_end_boss_id, Some(CONTINUATION));

            m.apply(&hp(12, 500, 500, FINAL, 10_000));
            m.apply(&hit(12, 100, 10_000, false));
            assert_eq!(m.snapshot(10_000).encounter.boss_monster_id, Some(FINAL));
            assert_eq!(m.fight_start_ms, Some(100), "one encounter throughout");
            assert_eq!(m.fight_state(10_100), FightState::Active);

            // The final phase's death latches through the ordinary
            // `boss_uid == target_uid` path — no fall-through to the idle
            // timeout — and the header holds on the phase just killed rather
            // than snapping back to the larger-max-hp corpse.
            m.apply(&hit(12, 100, 11_000, true));
            assert_eq!(m.fight_end_ms, Some(11_000));
            assert_eq!(m.fight_state(11_100), FightState::Ended);
            assert_eq!(m.snapshot(11_100).encounter.boss_monster_id, Some(FINAL));
            assert_eq!(m.snapshot(11_100).total_damage, 600);

            // issue #125: the dungeon's learned final boss converges on the
            // last phase engaged, which is the fight's real final phase.
            assert_eq!(m.scene_bosses.get(&DUNGEON_SCENE), Some(&FINAL));
        }

        // -- Part C: what may and may not clear an armed hold ---------------

        #[test]
        fn a_straggling_add_inside_the_window_does_not_clear_the_held_fight() {
            // PR #144 review, finding 1: the `NewFight` gate used to ask
            // nothing about the target, so a player AoE/DoT tick landing on
            // an unrelated add during the transition cutscene wiped the dead
            // phase's rows and restarted the clock — issue #124's own symptom,
            // inside the window built to prevent it.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            m.apply(&hp(12, 100, 100, TRASH, 2_000));
            let reason = m.apply(&hit(12, 50, 3_000, false));

            assert_eq!(reason, None, "an add is not the next pull");
            assert_eq!(m.fight_end_ms, Some(1_000), "the hold stays armed");
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            // ...and the real next phase still resumes into the same fight.
            m.apply(&hp(11, 500, 500, CONTINUATION, 20_000));
            m.apply(&hit(11, 700, 21_000, false));

            assert_eq!(m.fight_start_ms, Some(100));
            assert_eq!(m.snapshot(21_100).total_damage, 1_700);
        }

        #[test]
        fn the_next_phase_resumes_even_when_its_first_hit_beats_its_hp_packet() {
            // PR #144 review, finding 3: packet order is not guaranteed, so
            // the first swing at the next phase can decode before the
            // `EnemyHp` that names it. Treating that as a new fight was
            // unrecoverable — the reset drops `fight_end_boss_id`, so the
            // resume could never be retried once the id arrived.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            let reason = m.apply(&hit(11, 700, 21_000, false));

            assert_eq!(reason, None, "an unidentified target decides nothing");
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN), "still resumable");

            m.apply(&hp(11, 500, 500, CONTINUATION, 21_100));
            let reason = m.apply(&hit(11, 700, 21_200, false));

            assert_eq!(reason, None);
            assert_eq!(m.fight_start_ms, Some(100));
            assert_eq!(
                m.snapshot(21_300).total_damage,
                1_700,
                "the undecidable hit was held, not counted"
            );
        }

        #[test]
        fn a_missed_swing_on_the_next_phase_resumes_the_held_fight() {
            // PR #144 review, finding 4: neither the resume test nor the
            // `NewFight` gate looks at `is_miss`, so a whiffed opener on the
            // next phase resumes and counts a hit with no damage — exactly
            // what a miss does outside a phase change. Pinned because it is a
            // boundary someone will otherwise "fix" by accident.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            m.apply(&hp(11, 500, 500, CONTINUATION, 20_000));
            let reason = m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: 11,
                target_kind: EntityKind::Monster,
                value: 0,
                is_miss: true,
                timestamp_ms: 21_000,
                ..Default::default()
            }));

            assert_eq!(reason, None, "a miss is still the party engaging");
            assert_eq!(m.fight_start_ms, Some(100));
            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.snapshot(21_100).total_damage, 1_000);
            assert_eq!(m.snapshot(21_100).rows[0].hits, 3);
        }

        #[test]
        fn an_add_outside_the_window_still_clears_the_held_fight() {
            // The issue #78 contract the softening above must not eat: once
            // the resume window has expired, *any* player hit starts the next
            // fight, whatever it lands on.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));

            let late = 1_000 + window() + 1;
            m.apply(&hp(12, 100, 100, TRASH, late));
            let reason = m.apply(&hit(12, 50, late, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.fight_start_ms, Some(late));
            assert_eq!(m.snapshot(late + 100).total_damage, 50);
        }

        #[test]
        fn an_add_clears_a_hold_that_no_phase_change_could_resume() {
            // Same contract, the other way a hold can be unarmed: the boss
            // that ended the fight has no next phase at all, so nothing about
            // this hold is provisional.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, OTHER_BOSS, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            assert_eq!(m.fight_end_boss_id, Some(OTHER_BOSS));

            m.apply(&hp(12, 100, 100, TRASH, 2_000));
            let reason = m.apply(&hit(12, 50, 3_000, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(3_100).total_damage, 50);
        }

        #[test]
        fn an_add_clears_an_idle_timeout_hold_inside_the_window() {
            // And the third: an idle-timeout end leaves `fight_end_boss_id`
            // `None`, so the window never arms even though the boss that was
            // being fought is a phased one.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            assert_eq!(m.fight_state(100 + 20_000), FightState::Ended);
            assert_eq!(m.fight_end_boss_id, None);

            m.apply(&hp(12, 100, 100, TRASH, 20_000));
            let reason = m.apply(&hit(12, 50, 20_100, false));

            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(20_200).total_damage, 50);
        }

        // -- Part D: a corpse stays a corpse across a reset -----------------

        #[test]
        fn last_fights_corpse_cannot_block_the_next_bosss_latch() {
            // PR #144 review, finding 2. The boss dies to a death packet
            // while its last HP sync still reads above zero — the case
            // `mark_enemy_dead` exists for — so once `Meter::reset` cleared
            // `death_order`, `is_alive` fell back to that stale HP and the
            // corpse read as living for the whole next fight.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(10, 100, 200, true));
            assert_eq!(m.fight_end_ms, Some(200));
            assert_eq!(m.enemies[&10].curr_hp, Some(900), "no sync ever hit 0");

            m.reset(ResetReason::Manual, 300);

            // Next pull. A straggler DoT tick puts the corpse back into
            // `recompute_boss`'s pool alongside the boss actually being
            // fought.
            m.apply(&hit(10, 10, 400, false));
            m.apply(&hp(11, 500, 500, OTHER_BOSS, 400));
            m.apply(&hit(11, 100, 500, false));

            assert!(!m.enemies[&10].is_alive(), "the corpse is still dead");
            assert_eq!(m.boss_uid, Some(11), "the living boss keeps the header");

            m.apply(&hit(11, 100, 600, true));
            assert_eq!(m.fight_end_ms, Some(600), "and its death still latches");
        }

        #[test]
        fn a_respawned_boss_counts_as_living_again() {
            // The other half of finding 2's fix: what un-kills a corpse is a
            // real respawn — an HP sync above zero for an entity that has
            // taken no damage since the reset — not the reset itself.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(10, 100, 200, true));

            m.reset(ResetReason::Manual, 300);
            m.apply(&hp(10, 1_000, 1_000, ORIGIN, 400));

            assert_eq!(m.enemies[&10].death_order, None, "the rank is cleared");
            assert!(m.enemies[&10].is_alive());

            m.apply(&hit(10, 100, 500, false));
            m.apply(&hit(10, 100, 600, true));
            assert_eq!(m.fight_end_ms, Some(600), "the re-pull ends on its kill");
        }

        #[test]
        fn a_corpse_resyncing_upward_mid_fight_stays_dead() {
            // The `took_damage` gate on that respawn signal. Inside a fight a
            // dead phase's HP resyncing above zero is an artefact, and the
            // death latch must survive it — otherwise the corpse re-enters
            // `has_other_living_boss` and blocks the living phase's own end.
            let mut m = Meter::new();
            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            m.apply(&hit(10, 100, 200, true));
            assert_eq!(m.fight_end_ms, None, "the other phase is still up");

            m.apply(&hp(10, 1_500, 2_000, ORIGIN, 250));
            assert!(!m.enemies[&10].is_alive(), "a resync is not a respawn");

            m.apply(&hit(11, 100, 300, true));
            assert_eq!(m.fight_end_ms, Some(300));
        }

        // -- boss selection (issue #124 extends `recompute_boss`) -----------

        #[test]
        fn a_dead_recognized_boss_still_outranks_a_living_trash_add() {
            // The regression the key order exists to prevent: `recognized` is
            // compared before `alive`, so issue #78's post-kill header holds
            // on the boss instead of flipping to whatever straggler is still
            // swinging — even though the add is alive and has the larger HP
            // pool.
            let mut m = Meter::new();
            m.apply(&hp(10, 500, 500, ORIGIN, 0));
            m.apply(&hp(11, 9_000, 9_000, TRASH, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            m.apply(&hit(10, 100, 200, true));

            assert_eq!(m.boss_uid, Some(10));
            assert_eq!(m.snapshot(300).encounter.boss_monster_id, Some(ORIGIN));
            assert_eq!(m.fight_end_ms, Some(200), "the add cannot block the latch");
        }

        #[test]
        fn when_every_damaged_enemy_is_dead_the_last_one_killed_stays_selected() {
            // The ordinary end of a fight: `alive` is uniformly false, so
            // selection falls to the death order and holds on the phase the
            // party actually just finished. Without that key the larger-pool
            // first phase would win on `max_hp` and the frozen header would
            // name the wrong boss.
            let mut m = Meter::new();
            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            m.apply(&hit(10, 100, 200, true));
            m.apply(&hit(11, 100, 300, true));

            assert_eq!(m.boss_uid, Some(11));
            assert_eq!(
                m.snapshot(400).encounter.boss_monster_id,
                Some(CONTINUATION)
            );
        }

        #[test]
        fn a_single_boss_stays_selected_after_its_own_kill() {
            // The degenerate case the key order must leave untouched.
            let mut m = Meter::new();
            m.apply(&hp(10, 500, 500, ORIGIN, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(10, 100, 200, true));

            assert_eq!(m.boss_uid, Some(10));
            assert_eq!(m.snapshot(300).encounter.boss_monster_id, Some(ORIGIN));
        }

        #[test]
        fn selection_moves_to_the_living_phase_even_with_a_smaller_hp_pool() {
            let mut m = Meter::new();
            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            assert_eq!(m.boss_uid, Some(10), "both alive: the larger pool wins");

            m.apply(&hit(10, 100, 200, true));
            assert_eq!(m.boss_uid, Some(11), "the living phase takes over");
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

        // -- issue #151: the fight-end / reset diagnostics gap -------------

        #[test]
        fn fight_end_log_names_the_cause_and_the_boss() {
            let msg = fight_end_log(FightEndCause::BossDeath, Some(103));
            assert!(msg.contains("cause=boss_death"));
            assert!(msg.contains("boss_monster_id=103"));
            assert!(msg.contains("name=Rathalos"));

            let msg = fight_end_log(FightEndCause::IdleTimeout, Some(999_999));
            assert!(msg.contains("cause=idle_timeout"));
            assert!(msg.contains("<unresolved>"));

            let msg = fight_end_log(FightEndCause::Wipe, None);
            assert!(msg.contains("cause=wipe"));
            assert!(msg.contains("boss_monster_id=<unknown>"));

            let msg = fight_end_log(FightEndCause::ServerChanged, None);
            assert!(msg.contains("cause=server_changed"));
        }

        #[test]
        fn reset_log_reports_the_boss_hp_and_the_party_down_count() {
            // The pair issue #151 could not tell apart in a log: a rollback
            // with the party up...
            let msg = reset_log(ResetReason::BossHpRollback, Some(97.4), 0, 4);
            assert!(msg.contains("reason=BossHpRollback"));
            assert!(msg.contains("boss_hp_pct=97.4"));
            assert!(msg.contains("party_down=0/4"));

            // ...and the same shape with everyone dead.
            let msg = reset_log(ResetReason::NewFight, None, 4, 4);
            assert!(msg.contains("reason=NewFight"));
            assert!(msg.contains("boss_hp_pct=<unknown>"));
            assert!(msg.contains("party_down=4/4"));
        }

        #[test]
        fn fight_end_and_reset_logs_never_leak_a_player_name_or_uid() {
            let msg = reset_log(ResetReason::Manual, Some(50.0), 1, 4);
            assert!(!msg.contains("uid"));
            assert!(!msg.contains("Player"));
            let msg = fight_end_log(FightEndCause::Wipe, Some(103));
            assert!(!msg.contains("uid"));
        }
    }
}
