//! Encounter state machine: routes protocol events into per-player stats and
//! produces the UI-facing `Snapshot` (plan §T2.1/T2.2).

use std::collections::{BTreeMap, HashMap};

use crate::event::{
    CastEvent, Class, DamageEvent, DisappearReason, EDungeonState, EnemyHp, EntityId, EntityKind,
    PlayerInfo, ProtocolEvent,
};
use crate::fight::{FightConfig, FightEndCause, FightState};
use crate::phase;
use crate::reset::{EnemyState, ResetConfig, ResetReason, check_hp_rollback};
use crate::stats::{
    ActiveBuff, EncounterInfo, PlayerRow, PlayerStats, SkillRow, SkillStats, Snapshot,
};
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

/// How long after the last hit on a recognized boss that boss still counts
/// as the one the fight is about, for `Meter::recompute_boss`'s issue #157
/// fallback (PR #163 re-review).
///
/// The fallback exists because `Meter::reset` clears `took_damage` on every
/// enemy, so for the instant after a mid-pull reset — a wipe-and-re-pull's
/// `BossHpRollback`, a `Manual` reset, the `NewFight` reset that starts the
/// next attempt — the first thing hit wins `boss_entity` outright, and party
/// AoE lands on adds first. It therefore has to reach back past the reset,
/// to an enemy with no damage *in this fight at all*. That reach is what
/// needs bounding: "alive and damaged at some point" also describes a boss
/// the party gave up on and left standing, since `EnemyState::is_alive`
/// reads a boss never observed dying as alive forever, and nothing but a
/// `ServerChanged` ever clears `enemies`.
///
/// Nothing structural separates those two cases — both are "damaged, then a
/// reset, then something else takes a hit" — so the separator is elapsed
/// time, and the window has to clear the longest lull *inside* a pull. That
/// rules out `FightConfig::idle_timeout_ms`: at 9s it is deliberately
/// shorter than a raid's immunity and mechanic windows (see its own doc
/// comment), so a boss going untargetable while the party burns an add wave
/// would hand the header to the adds — issue #157's bug. It also rules out
/// keying off fight or reset boundaries: the re-pull after a wipe crosses a
/// fight end and a reset exactly like walking away does.
///
/// 60s is sized like `FightConfig::phase_resume_window_ms`, and for the same
/// reason: long enough for a cutscene, a corpse run or an add phase plus the
/// window before the boss can be hit again, far shorter than the gap before
/// a party that abandoned a boss finds its next pull. Being a bound on a
/// heuristic's blast radius rather than a user-visible behaviour, it is not
/// a `FightConfig` tunable — a zero there would silently revert issue #157.
const BOSS_ENGAGEMENT_WINDOW_MS: u64 = 60_000;

/// How long a party wipe's attempt is held for review before the *next*
/// player damage of any kind is allowed to start a fresh fight (issue #204).
///
/// Issue #154 froze a wipe and made only one thing lift the freeze: a player
/// damaging a target whose cached `monster_id` resolves through
/// `tables::is_boss_monster` (see `withholds_after_wipe`). That is the right
/// test for the run-back — an AoE clipping an add on the way back in is not
/// the next pull — but as the *only* test it has no floor. Nothing
/// guarantees the re-pull ever presents a recognizable boss: the respawn can
/// come up under a fresh uid whose `EnemyHp` never arrives, the party can
/// give up and pull something else, or the boss's id can simply be missing
/// from the generated table. In every one of those the hold never lifts, and
/// because `apply_damage` returns early for the whole duration of a hold,
/// every hit, death and point of damage after it is silently dropped — the
/// meter shows the wiped attempt's frozen elapsed timer until the player
/// zones, reconnects or resets by hand. That is issue #204 as reported.
///
/// So the recognized-boss test keeps deciding, but only for as long as the
/// attempt is plausibly still *being reviewed*. Past this bound, the hold has
/// outlived its purpose and the ordinary issue #78 rule takes back over:
/// the next real player hit starts the next fight, whatever it lands on.
/// Anchored on `fight_end_ms` — the wipe itself — rather than on the last
/// event, so nothing that happens during the hold can push the release out
/// (issue #155's boss swinging at the corpses least of all).
///
/// 60s, sized like [`BOSS_ENGAGEMENT_WINDOW_MS`] and
/// `FightConfig::phase_resume_window_ms`: longer than any corpse run plus the
/// trash on the way back — which is what issue #154's guarantee costs — and
/// far shorter than a user's patience with a meter that has stopped
/// responding. Not a `FightConfig` tunable for the same reason
/// `BOSS_ENGAGEMENT_WINDOW_MS` is not: a zero would silently reinstate the
/// wedge this exists to bound.
const WIPE_HOLD_RELEASE_MS: u64 = 60_000;

/// Fraction of the roster that must be down *at the instant a boss's HP bar
/// rolls back* for that rollback to be read as a wipe rather than as a bare
/// reset (issue #259).
///
/// The damage-event wipe path ([`Meter::party_is_wiped`]) demands the whole
/// roster be down, and has to: a death packet on its own says nothing about
/// whether the pull is over, so anything short of unanimity there would
/// freeze the meter mid-fight. The rollback path carries that second signal
/// itself — the boss the party burned below `hp_drop_below_pct` is back at
/// `hp_rollback_at_pct` or above, which is the server resetting the
/// encounter, i.e. the pull is over as a matter of fact rather than of
/// inference. With that in hand the roster no longer has to be unanimous,
/// and demanding that it be is what made issue #259's outcome a coin flip:
/// whether the attempt was recorded depended on whether the last death
/// packet happened to land before the HP sync did.
///
/// Four in five — 12 of a 15-player raid — because the roster is not
/// exactly "the party still fighting": it can hold a straggler who is
/// genuinely up (a healer out of range of whatever finished the group, a
/// player battle-rezzed seconds before the server gave up on the pull, or
/// a row `apply_damage` opened for someone outside the party), and each of
/// those alone must not veto the wipe. Three such rows in a fifteen-player
/// raid is the headroom this buys.
///
/// Deliberately measured against `players.len()` rather than a party size
/// from the roster packet: `players` is the only roster this crate has, and
/// it is what both wipe paths already read. Note that the `party_down=N/M`
/// figure in the `reset`/issue #151 log lines counts players with
/// `deaths > 0` — cumulative, per issue #212 — so it is an upper bound on
/// how many were down at any one instant and cannot be used to calibrate
/// this constant directly.
const WIPE_PARTY_DOWN_FRACTION: f64 = 0.8;

/// The most health an enemy may have been last seen with for its despawn to
/// be readable as a death (issue #215), as a percentage of its pool.
///
/// **The fallback, since issue #276.** A `pb::DisappearEntity` that carries
/// tag 2 states its own reason and this threshold is never consulted — see
/// [`Meter::apply_enemy_gone`]. But tag 2 is optional and 382 of 851 captured
/// disappear entries (23 of them monsters) carry none at all, and for those
/// "the corpse was removed" and "the player walked out of AOI range" still
/// arrive on the wire as literally the same bytes. Everything else the rule
/// tests is about *which* enemy vanished; this is the only condition left
/// that speaks to whether it plausibly *died*. A boss at 3% of its bar that
/// stops being mentioned is the tail of a kill whose death packet went
/// missing; the same boss at 80% is a party that ran away.
///
/// 10% rather than something looser because the two error directions are not
/// symmetric — the same asymmetry `EnemyState::is_alive` is built on, pointed
/// the other way. Refusing a real death costs only the instant freeze: the
/// idle timeout still ends the fight, bounded by
/// [`BOSS_ENGAGEMENT_WINDOW_MS`] since issue #210/#211. Accepting a range-out
/// as a death ends a live pull early, saves a truncated encounter, and splits
/// the rest of it into a second one — strictly worse than the behaviour this
/// replaces. A percentage rather than an absolute figure because it has to
/// hold across pools three orders of magnitude apart, and it reuses
/// `EnemyState::pct`, which measures against the observed peak when `max_hp`
/// was never synced.
///
/// Not a `FightConfig` tunable, for the same reason `BOSS_ENGAGEMENT_WINDOW_MS`
/// is not: it bounds a heuristic's blast radius rather than expressing a
/// user-visible preference, and a 100 there would turn every AOI eviction
/// into a boss death.
const DESPAWN_DEATH_MAX_HP_PCT: f64 = 10.0;

/// Whether an enemy last damaged at `last_damaged_ms` was still being
/// fought when the enemy now holding the target was hit at `engaged_at`,
/// per [`BOSS_ENGAGEMENT_WINDOW_MS`].
///
/// An enemy that has never been damaged is never in the window — that is
/// the AOI-only boss of PR #163 review finding 2. A missing `engaged_at`
/// means the current target has no damage clock to compare against (only
/// reachable from a hand-built `EnemyState`), so the check degrades to the
/// plain "ever damaged" question rather than dropping the fallback.
/// Damage *newer* than `engaged_at` — an `EnemyHp` packet arriving between
/// two hits — is trivially inside the window, not outside it.
///
/// Also used by `Meter::engaged_boss_still_up` (issue #210/#211), with
/// `engaged_at` there being the caller's own `now_ms` rather than another
/// enemy's damage clock — the same "is this recent enough to still count as
/// the pull in progress" question, just measured against wall-clock time
/// instead of a sibling boss's last hit.
fn engaged_within_window(last_damaged_ms: Option<u64>, engaged_at: Option<u64>) -> bool {
    match (last_damaged_ms, engaged_at) {
        (Some(last), Some(now)) => now.saturating_sub(last) <= BOSS_ENGAGEMENT_WINDOW_MS,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Whether `e` is a recognized boss (`tables::is_boss_monster`) the party
/// has actually engaged this fight and that is not known to be dead.
///
/// The identity half of the question every boss-liveness guard asks —
/// [`Meter::engaged_boss_still_up`], [`Meter::other_living_boss`] and
/// [`Meter::apply_enemy_gone`] all start here. `took_damage` is what scopes
/// it to the current encounter (a boss standing in a room the party walked
/// past was never part of one), and "not known to be dead" is
/// [`EnemyState::is_alive`], which reads never-observed health as alive.
///
/// Deliberately says nothing about *recency*: the callers disagree there.
/// Two of them always require [`engaged_within_window`] on top (see
/// [`is_engaged_recognized_boss`]); `other_living_boss` requires it only
/// inside a boss-select scene, where a sequential next selection must not
/// hold the current one's death open.
fn is_damaged_living_boss(e: &EnemyState) -> bool {
    e.took_damage && e.is_alive() && e.monster_id.is_some_and(tables::is_boss_monster)
}

/// [`is_damaged_living_boss`] plus the recency half: the party was hitting
/// `e` within [`BOSS_ENGAGEMENT_WINDOW_MS`] of `now_ms` (issue #210/#211),
/// so it is a pull genuinely in progress rather than one abandoned — or a
/// boss whose death signal was simply never delivered, which
/// `EnemyState::is_alive` would otherwise read as alive forever.
fn is_engaged_recognized_boss(e: &EnemyState, now_ms: u64) -> bool {
    is_damaged_living_boss(e) && engaged_within_window(e.last_damaged_ms, Some(now_ms))
}

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
    /// Each equipped slot's tier (issues #169/#170). Cached alongside
    /// `imagines` for the same reason and under the same live-wins rule —
    /// see `apply_cached_attrs`/`name_upsert`.
    imagine_tiers: Option<[Option<i32>; 2]>,
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
    /// Each equipped slot's tier (issues #169/#170) — see `NameEntry`'s
    /// field of the same name.
    imagine_tiers: Option<[Option<i32>; 2]>,
}

/// Copies the six cacheable identity fields (name/class/ability_score/
/// season_strength/imagines/imagine_tiers) from `merged` onto `stats`, one
/// at a time, skipping any field `merged` has no opinion on (issue #145
/// finding 6: this list used to be duplicated between `apply_player`'s
/// existing-row and preload branches, so adding a sixth cached field meant
/// editing both). A freshly `PlayerStats::new` row starts with every one of
/// these fields at `None`, so the per-field guard is a no-op there — this
/// same guarded copy is exactly equivalent to an unconditional one for a
/// fresh row, which is what lets both branches share it.
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
    if merged.imagine_tiers.is_some() {
        stats.imagine_tiers = merged.imagine_tiers;
    }
}

/// The key a damage event's attacker is filed under (issue #335): its own
/// whole-uuid identity, or — for an event built from a display uid alone —
/// the canonical reconstruction for that uid. See
/// [`EntityId::or_display`].
fn attacker_key(d: &DamageEvent) -> EntityId {
    d.attacker.or_display(d.attacker_uid, d.attacker_kind)
}

/// The key a damage event's target is filed under (issue #335).
fn target_key(d: &DamageEvent) -> EntityId {
    d.target.or_display(d.target_uid, d.target_kind)
}

pub struct Meter {
    /// Per-player state, keyed on the whole-uuid [`EntityId`] rather than the
    /// truncated display uid (issue #335). The display uid is not unique —
    /// a server session recycles it, and a shadow/mirror entity shares one
    /// with its original — so keying on it silently blended two players'
    /// stats into one row. Rows still *print* `PlayerStats::uid`, and the
    /// name cache below is still keyed on it, because that is the number
    /// the game itself shows.
    players: HashMap<EntityId, PlayerStats>,
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
    /// Per-enemy state, keyed on the whole-uuid [`EntityId`] for the same
    /// reason `players` is (issue #335).
    enemies: HashMap<EntityId, EnemyState>,
    fight_start_ms: Option<u64>,
    /// Timestamp of the most recent event seen (damage or enemy-hp). Used as
    /// the DPS-window end and as the reference point for the boss-HP-rollback
    /// cooldown gate.
    last_event_ms: u64,
    /// Timestamp of the last reset, if any. `None` means no reset has
    /// happened yet, so the cooldown gate never blocks the first rollback.
    last_reset_ms: Option<u64>,
    boss_entity: Option<EntityId>,
    /// Current dungeon/instance id (issue #9 slice 2), from the most recent
    /// `ProtocolEvent::Scene`. Survives `Meter::reset` (a manual reset or a
    /// boss-HP rollback both stay in the same dungeon); cleared only on
    /// `ServerChanged`, in `apply` directly rather than in `reset` itself.
    scene_id: Option<u32>,
    /// The most recent scene id an actual `ProtocolEvent::Scene` reported,
    /// **not** cleared by `ServerChanged` (issue #295). `scene_id` goes
    /// `None` across a reconnect because the destination is genuinely
    /// unknown until the next `Scene` packet — but by the time that packet
    /// arrives, comparing its id against the *previous* confirmed scene is
    /// no longer ambiguous, and every real capture sends `ServerChanged`
    /// immediately before the `Scene` that follows a zone transition. Using
    /// `scene_id` itself for that comparison (as the `Scene` arm's
    /// `entering_dungeon` gate once did) meant the comparison always saw
    /// `None` in production and could never tell a genuinely new dungeon
    /// from a late reconnect-confirmation of the one just left — so the
    /// fast `SceneChanged` reset never fired outside a unit test, and a
    /// fight held since the previous instance sat un-reset until either
    /// real combat (`NewFight`) or that dungeon's own `Playing` packet
    /// (`DungeonStarted`) eventually caught it, sometimes minutes later.
    last_known_scene_id: Option<u32>,
    /// When the current fight ended, if it has (issue #78). `Some(t)` puts
    /// the meter in [`FightState::Ended`]: the snapshot is rendered as of
    /// `t` rather than the caller's `now_ms`, so rows, totals and the
    /// elapsed timer all hold still until the next fight (or a manual reset
    /// / server change) clears them. Latched by an explicit end signal (a
    /// boss death) or by [`Meter::tick`] once the idle timeout has elapsed;
    /// cleared by `reset`.
    fight_end_ms: Option<u64>,
    /// When the fight end recorded in `fight_end_ms` was actually *latched*
    /// — i.e. the argument `now_ms`/`d.timestamp_ms` the call into
    /// [`Meter::latch_fight_end`] carried — as opposed to `fight_end_ms`
    /// itself, which for an idle-timeout end is the last *player* hit, not
    /// "now" (issue #316).
    ///
    /// Those two used to be the same value everywhere `phase_resume_window_ms`
    /// read from, and that coupling made phase resumption structurally
    /// unreachable for a recognized boss's idle-timeout end: idle detection
    /// is suppressed for as long as [`Meter::engaged_boss_still_up`] holds
    /// (up to [`BOSS_ENGAGEMENT_WINDOW_MS`] past the last hit), so by the
    /// time the end is actually observed and latched, `fight_end_ms` is
    /// already `BOSS_ENGAGEMENT_WINDOW_MS` in the past — and at stock config
    /// the two windows are equal, leaving zero budget for
    /// `FightConfig::phase_resume_window_ms` to ever still be open.
    /// Anchoring the resume window on *this* field instead — when the end
    /// was observed, not when the fight clock says it happened — fixes that
    /// without touching either window's size or default. `ServerChanged`,
    /// `Wipe`, `BossDeath` and `DungeonEnded` ends are all latched
    /// immediately (their `end_ms` already *is* "now"), so this only ever
    /// diverges from `fight_end_ms` on the idle-timeout path.
    ///
    /// Cleared everywhere `fight_end_ms` is.
    fight_end_observed_ms: Option<u64>,
    /// The monster id whose death latched `fight_end_ms`, if that is what
    /// ended the fight (issue #124). This is what arms phase resumption: a
    /// dungeon's final boss can move through several phases, each a distinct
    /// monster id whose predecessor really dies, and the first hit on the
    /// next phase must resume the held fight rather than reset it.
    ///
    /// Also set by an idle-timeout end (issue #316), naming whichever
    /// recognized boss this fight had engaged and not seen die — see
    /// [`Meter::engaged_boss_monster_id`]. Before #316 this was left `None`
    /// on that path, on the theory that walking away from a pull and coming
    /// back to a same-family boss should start a new fight; in practice a
    /// recognized boss's idle-timeout end is only reachable once
    /// `engaged_boss_still_up` releases it, which already means the party
    /// hasn't touched it in `BOSS_ENGAGEMENT_WINDOW_MS` — leaving this
    /// unarmed made every idle-timeout end on a phased boss indistinguishable
    /// from truly walking away, so a mid-transition hit on the next phase
    /// (immunity phase, add wave, cutscene) reset the encounter instead of
    /// continuing it. Cleared by `reset` (and so by the `ServerChanged`
    /// path, which resets first), and by `ServerChanged`/dungeon-entry
    /// directly wherever `enemies` is cleared (issue #316) — those clear the
    /// map `Self::target_monster_id` reads to decide whether a post-reconnect
    /// hit is a phase change, and a stale armed id there withheld every hit
    /// until the window lapsed on its own.
    fight_end_boss_id: Option<u32>,
    /// Whether the fight was ended by a **party wipe** and the attempt is
    /// being held for review (issue #154). A wipe is the end of a pull, not
    /// a reset: the rows freeze exactly as they do on a boss kill, and this
    /// flag is what makes the hold ignore *everything* until the party is
    /// truly re-engaged — the boss's bar refilling, its swings at the
    /// corpses, an AoE tick clipping an add on the run-back. Only a player
    /// damaging a recognized boss again lifts it, through the ordinary
    /// `NewFight` path (see `withholds_after_wipe`) — or, once the attempt
    /// has been held for [`WIPE_HOLD_RELEASE_MS`], any player damage at all,
    /// so a re-pull the meter cannot recognize can never wedge the hold open
    /// forever (issue #204).
    ///
    /// Cleared by `reset` — so by that same `NewFight` — and by a server
    /// change, which invalidates the entity state the re-engagement test
    /// reads and hands the hold back to issue #78's ordinary rule.
    wipe_hold: bool,
    /// Identity of the fight currently on the board (issue #152): the
    /// recognized boss it is against and the scene it is being fought in,
    /// captured while the fight is *live* by `recompute_boss`.
    ///
    /// `snapshot` renders this instead of live state for as long as the
    /// fight is held ([`FightState::Ended`]). Zoning out of a dungeon
    /// discards the live answer — `ServerChanged` clears `enemies`,
    /// `boss_entity` and `scene_id`, then the town's `Scene` event lands —
    /// while the fight's rows, totals and clock stay frozen on screen, so
    /// without this capture the header would caption a raid's damage
    /// breakdown with "No target" and the name of the town the player just
    /// walked into. Cleared by `reset`, i.e. released exactly when the hold
    /// itself is (next fight, manual reset, HP rollback).
    fight_identity: Option<FightIdentity>,
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
    /// Current dungeon lifecycle state (issue #139), from the most recent
    /// `ProtocolEvent::DungeonState`. `None` until the first such event
    /// arrives this session — the case a plain open-world session, or one
    /// on a build that never sends `0x17`/`0x18`, never leaves. Instance-
    /// level state, like `scene_id`: survives `Meter::reset` (a
    /// `DungeonStarted` reset or a raid-boss-reset both stay inside the
    /// same instance), and is cleared only when the instance itself goes
    /// away — an explicit `DungeonState::Null` (§4), a `ServerChanged`, or
    /// entering a different dungeon/raid scene (mirroring the `enemies`/
    /// `boss_entity` clears at both of those points).
    dungeon_state: Option<EDungeonState>,
    /// Every dungeon objective seen this instance, keyed by target id
    /// (issue #139 §1). Survives `Meter::reset` for the same reason
    /// `dungeon_state` does: the raid-boss reset detector (§6) and the
    /// boss-death gate (§8) both need this instance's objective history to
    /// outlive the very resets they can trigger. Cleared at the same three
    /// points as `dungeon_state`.
    objectives: BTreeMap<i32, ObjectiveState>,
    /// The first objective id observed this instance (issue #139 §6) — the
    /// raid's first boss/target. Set once, on the first `DungeonObjective`
    /// transition, and never overwritten afterward until the instance-level
    /// clear points above. The raid-boss reset detector fires when
    /// `current_objective_id` returns to this value after having moved off
    /// it: "one raid boss killed while others are unbeaten" (issue #210).
    first_objective_id: Option<i32>,
    /// The objective the instance is currently on (issue #139 §5),
    /// updated by each recognized transition. `None` until the first one.
    current_objective_id: Option<i32>,
}

/// Last known `nums`/`complete` for one dungeon objective/target (issue
/// #139), keyed by target id in `Meter::objectives`. Each field mirrors
/// `ProtocolEvent::DungeonObjective`'s identically-named `Option` — an
/// update commonly carries only one of the two (see the wire format's
/// `TargetData`), so `Meter::apply_dungeon_objective` only overwrites a
/// field the incoming event actually set, never clobbering an
/// already-known value back to unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ObjectiveState {
    nums: Option<i32>,
    complete: Option<bool>,
}

/// Which fight a held snapshot belongs to (issue #152). Ids only: the
/// display names are pure functions of them (`tables::monster_name`,
/// `tables::scene_name`), so storing the names too would just be a second
/// copy that can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FightIdentity {
    /// The `tables::is_boss_monster` id the header was naming — whichever
    /// boss `recompute_boss` had selected, which is not necessarily the only
    /// one that was up (a raid selection can put two equal-HP bosses on the
    /// field at once). A trash pull has no identity worth pinning, so
    /// `recompute_boss` never records one.
    boss_monster_id: u32,
    /// The scene the fight was in, `None` if it was never known.
    scene_id: Option<u32>,
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
            boss_entity: None,
            scene_id: None,
            last_known_scene_id: None,
            fight_end_ms: None,
            fight_end_observed_ms: None,
            fight_end_boss_id: None,
            wipe_hold: false,
            fight_identity: None,
            deaths_seen: 0,
            reset_cfg: ResetConfig::default(),
            fight_cfg: FightConfig::default(),
            preload_count: 0,
            dungeon_state: None,
            objectives: BTreeMap::new(),
            first_objective_id: None,
            current_objective_id: None,
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
                    imagine_tiers: None,
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
                imagine_tiers: entry.imagine_tiers,
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
        if incoming.imagine_tiers.is_some() {
            entry.imagine_tiers = incoming.imagine_tiers;
        }
        entry.seq = seq;
        CachedAttrs {
            name: entry.name.clone(),
            class: entry.class,
            ability_score: entry.ability_score,
            season_strength: entry.season_strength,
            imagines: entry.imagines,
            imagine_tiers: entry.imagine_tiers,
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
            && !self.engaged_boss_still_up(now_ms)
        {
            Some(self.last_event_ms)
        } else {
            None
        }
    }

    /// Whether `timestamp_ms` falls inside the post-end grace window
    /// (`FightConfig::post_end_grace_ms`) trailing the currently-held
    /// fight's end.
    ///
    /// `false` while a fight is still running (`fight_end_ms` is `None`) —
    /// grace only ever widens what an *ended* fight accepts, it is not a
    /// second way to end one. A timestamp at or before `fight_end_ms`
    /// itself counts too (`saturating_sub` floors at 0): a delayed-arrival
    /// packet stamped a moment before the fight actually ended is exactly
    /// the case this exists for, not an edge case to exclude.
    ///
    /// **Inclusive at the far edge too**: `end_ms + post_end_grace_ms`
    /// itself is still inside the window. `bpsr_app`'s history recorder
    /// (`Pipeline::record_fight_end`) shares that convention from the other
    /// side, treating the window as closed only once `now` is *strictly
    /// past* that instant — otherwise it would flush a record while this
    /// was still folding packets into the fight it describes. Change one
    /// and the other has to change with it.
    ///
    /// Only the timestamp half of the grace test: safe as-is for
    /// `apply_cast`/`apply_buff_apply`/`apply_buff_remove`, none of which
    /// can trigger the `NewFight` reset regardless of what they target (a
    /// cast/buff event carries no monster target at all). `apply_damage`
    /// needs the stronger, target-aware [`Self::damage_in_post_end_grace`]
    /// instead — see its doc comment for why.
    fn in_post_end_grace_window(&self, timestamp_ms: u64) -> bool {
        self.fight_end_ms.is_some_and(|end_ms| {
            timestamp_ms.saturating_sub(end_ms) <= self.fight_cfg.post_end_grace_ms
        })
    }

    /// Whether `d` is a trailing packet of the fight that just ended,
    /// rather than the first sign of a new one — the question
    /// `apply_damage` needs answered both to decide whether the `NewFight`
    /// reset may fire and, if the event is withheld some other way, whether
    /// it should still be folded into the ended fight's stats
    /// (`apply_damage_grace`).
    ///
    /// A heal, or any event a monster attacker deals, can never single-
    /// handedly trigger that reset in the first place (see `apply_damage`'s
    /// gate: `d.attacker_kind == EntityKind::Player && !d.is_heal`), so
    /// grace for those needs only the timestamp check.
    ///
    /// A real (non-heal) player hit is different: on its own it is exactly
    /// the signal issue #78's `NewFight` reset exists to catch, and a
    /// timestamp alone cannot tell "the last DoT tick of the boss that just
    /// died" from "the opening hit of an unrelated add, or the next pull,
    /// that merely happens to land inside the window" — both PR #144's
    /// straggling-add case and the reference implementation's own scope
    /// (trailing packets of *this* encounter) are about the former, not the
    /// latter. So this additionally requires the target to be an enemy this
    /// fight had already damaged (`EnemyState::took_damage`) before it
    /// ended: true of the boss the party was just hitting, false of an add,
    /// the next dungeon's first pull, or a post-`ServerChanged` reconnect's
    /// first swing on a uid this session has never named.
    fn damage_in_post_end_grace(&self, d: &DamageEvent) -> bool {
        if !self.in_post_end_grace_window(d.timestamp_ms) {
            return false;
        }
        if d.attacker_kind != EntityKind::Player || d.is_heal {
            return true;
        }
        self.enemies
            .get(&target_key(d))
            .is_some_and(|e| e.took_damage)
    }

    /// Whether the party is mid-pull on a boss that simply is not being hit
    /// right now (issue #151): *any* recognized boss
    /// (`tables::is_boss_monster`) that has taken damage this fight and is
    /// not known to be dead.
    ///
    /// Deliberately "any", not "`boss_entity`": a pull can have two bosses up
    /// at once (Dreambloom Ruins' Caprahorn spawns a matched pair fought
    /// together), and `boss_entity` can only name one of them. If the named
    /// one dies first, the pull is still very much in progress.
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
    /// and a scene change latches it at the last hit (issue #191, see the
    /// `SceneChanged` arm in `apply`) — zoning out never leaves a held
    /// fight running. Failing all three, the engagement window below ends
    /// it on its own.
    ///
    /// Deliberately scoped by `took_damage`, like `other_living_boss`:
    /// a boss standing in the room the party walked past is not a pull in
    /// progress.
    ///
    /// Bounded by [`BOSS_ENGAGEMENT_WINDOW_MS`] against `last_damaged_ms`
    /// (issue #210/#211): the meter has exactly two death signals
    /// (`DamageEvent::is_dead` and an `EnemyHp` sync to `curr_hp == Some(0)`),
    /// and production logs show a boss can simply never produce either —
    /// no death packet, no zero-HP sync, it just stops being mentioned (a
    /// dropped TCP segment, most likely). `EnemyState::is_alive` reads that
    /// boss as alive forever, which used to make this `true` forever too,
    /// permanently suppressing the only fallback that could still end the
    /// fight (the idle timeout) — the meter never saved the encounter, reset
    /// the meter, or stopped the clock, and the *next* boss's damage
    /// accumulated into the dead one's rows for as long as the instance
    /// stayed open. Bounding this to `now_ms` recent still covers every
    /// genuine immunity/mechanic lull the guard exists for (real windows
    /// are far shorter than 60s), while turning a missed death signal from
    /// a permanent wedge into one bounded the same way a boss nobody has
    /// ever touched already is.
    ///
    /// Issue #313: this used to require `in_dungeon_scene()` as well, and
    /// scene 7152 ("World Dominator") — a world-boss arena that is not a
    /// `tables::DUNGEON_SCENE_IDS` instance — is what exposed that as
    /// wrong: an arena boss went invulnerable mid-phase, the 9s idle
    /// timeout ran out unopposed, and the party's next hit fired
    /// `ResetReason::NewFight` on a pull still 41.8% from done. The scene
    /// check dates from PR #163, *before* `BOSS_ENGAGEMENT_WINDOW_MS`
    /// existed: back then "damaged and alive" was unbounded, so a boss
    /// whose death signal never arrived read alive forever and the scene
    /// check was the only valve that could ever release the suppression.
    /// The window carries that whole bound now — the guard self-releases
    /// 60s after the last hit, and `fight_ended_at` then ends the fight
    /// retroactively at `last_event_ms`, fabricating no elapsed time — so
    /// the scene check was pure vestige. The accepted cost is that out in
    /// the open world a fight with an engaged recognized boss is now held
    /// open up to 60s past the last hit instead of 9s: the same trade
    /// every dungeon pull already makes, and the price of not wiping a
    /// live world-boss pull.
    ///
    /// Note this is the *idle* path's judgement only. The issue #154 wipe
    /// hold keeps its own `in_dungeon_scene()` gate at its call site (PR
    /// #163 review, finding 1) — that gate has an instance justification of
    /// its own, and is not widened here.
    fn engaged_boss_still_up(&self, now_ms: u64) -> bool {
        self.enemies
            .values()
            .any(|e| is_engaged_recognized_boss(e, now_ms))
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

    /// When the currently-held fight ended, in the event clock's milliseconds —
    /// `None` while a fight is running or none has happened since the last reset
    /// (issue #39). Only meaningful once `fight_state`/`tick` report
    /// [`FightState::Ended`], which is also what latches the value.
    ///
    /// The event clock *is* wall-clock Unix milliseconds in production
    /// (`bpsr_app::pipeline::now_ms` stamps every event), and scripted values in
    /// the replay tests — which is exactly what makes the history goldens
    /// deterministic. Callers persisting this as a timestamp are relying on that
    /// and on nothing else.
    pub fn fight_end_ms(&self) -> Option<u64> {
        self.fight_end_ms
    }

    /// When the currently-running (or currently-held) fight started, in the
    /// same event-clock milliseconds as [`Self::fight_end_ms`] — `None` when
    /// no fight has begun since the last reset.
    ///
    /// Stable for the whole life of one fight, *including across a phase
    /// resume*: [`Self::resumes_held_fight`] deliberately leaves it alone
    /// (issue #124), while a `ResetReason::NewFight` moves it and a reset
    /// clears it. That makes it the identity of "this fight" for callers
    /// that have to tell a resumed fight from a brand-new one —
    /// `bpsr_app::pipeline::Pipeline::record_fight_end` is exactly that
    /// caller.
    pub fn fight_start_ms(&self) -> Option<u64> {
        self.fight_start_ms
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
            self.latch_fight_end(
                FightEndCause::IdleTimeout,
                end_ms,
                now_ms,
                self.boss_monster_id(),
            );
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
            ProtocolEvent::Cast(c) => {
                self.apply_cast(c);
                None
            }
            ProtocolEvent::Player(p) => {
                self.apply_player(p);
                None
            }
            ProtocolEvent::EnemyHp(e) => self.apply_enemy_hp(e),
            ProtocolEvent::BuffApply {
                host,
                host_uid,
                buff_uuid,
                base_id,
                adds_layer,
                timestamp_ms,
            } => {
                self.apply_buff_apply(
                    host.or_display(*host_uid, EntityKind::Player),
                    *buff_uuid,
                    *base_id,
                    *adds_layer,
                    *timestamp_ms,
                );
                None
            }
            ProtocolEvent::BuffRemove {
                host,
                host_uid,
                buff_uuid,
                removes_layer,
                timestamp_ms,
            } => {
                self.apply_buff_remove(
                    host.or_display(*host_uid, EntityKind::Player),
                    *buff_uuid,
                    *removes_layer,
                    *timestamp_ms,
                );
                None
            }
            ProtocolEvent::Scene { level_map_id } => {
                // Sparse, transition-only diagnostic (issue #69): a scene
                // sync packet can repeat while the player stays in the same
                // instance, so log only when the resolved id actually
                // changes — never per packet, which would just be a smaller
                // version of the #87 flood this exists to avoid.
                if let Some(msg) = scene_transition_log(self.scene_id, Some(*level_map_id)) {
                    log::info!("{msg}");
                }
                // issue #191: a repeat sync reporting the *same* scene id
                // is not a real transition — issue #78's hold is untouched
                // by it, which is what keeps a just-finished fight's
                // numbers on screen for the user to screenshot as long as
                // they're in the instance it was fought in.
                let mut reason = None;
                // issue #293: a genuinely first-ever scene learn — neither
                // `scene_id` (this session) nor `last_known_scene_id`
                // (survives `ServerChanged`, see issue #295 above) has ever
                // been set — must not be treated as a scene *change*. That
                // is exactly what a mid-instance attach looks like: there
                // was no `ENTER_SCENE` to see (it fired once, before the
                // meter existed), so the first `Scene` this session ever
                // gets is `SyncContainerData`'s full-state push, which can
                // land well after damage already has. Below this guard
                // assumes a real transition — `cut_short`/`latch_fight_end`
                // would otherwise stamp a fight still genuinely in progress
                // as cut short by a "departure" that never happened, the
                // instant a late-attaching meter finally learns where it
                // is. `ServerChanged` having cleared `scene_id` is *not*
                // this case — `last_known_scene_id` is still set then, so
                // the check below still runs and issue #295's fast reset
                // still fires on a confirmed different scene.
                let first_ever_scene_learn =
                    self.scene_id.is_none() && self.last_known_scene_id.is_none();
                if !first_ever_scene_learn && self.scene_id != Some(*level_map_id) {
                    // issue #12: drop preloaded roster rows nobody ever
                    // damaged, logging a summary first — a stale party
                    // member from the last run must not linger even in the
                    // sliver of time before the id below actually lands.
                    self.prune_stale_preloads();

                    // issue #191: a fight still running when the scene
                    // changes out from under it never gets to end on its
                    // own — latch it here, exactly as the `ServerChanged`
                    // arm below already does for the same reason (issue
                    // #138), but under its own `SceneChanged` cause: an
                    // ordinary same-shard dungeon transition is not a
                    // reconnect, and stamping it `server_changed` would give
                    // false hits to anyone grepping issue #151's fight-end
                    // diagnostic for connection bugs. Timestamped by the
                    // last real damage, not "now"
                    // (see `fight_ended_at`): the fight really ended when
                    // the hitting stopped, not whenever this packet
                    // happened to arrive. Applies regardless of the
                    // destination — a fight cut short by a same-shard
                    // transition into the open world deserves to freeze and
                    // be recorded too, even when the reset below won't fire
                    // for it.
                    let cut_short = self.fight_start_ms.is_some() && self.fight_end_ms.is_none();
                    if cut_short {
                        self.latch_fight_end(
                            FightEndCause::SceneChanged,
                            self.last_event_ms,
                            self.last_event_ms,
                            self.boss_monster_id(),
                        );
                    }

                    // issue #191: only entering a *dungeon/raid* scene
                    // clears the roster immediately. That is the one
                    // transition where new preloaded rows are about to
                    // start landing — `apply_player`'s preload path is
                    // gated on `in_dungeon_scene`, i.e. this exact same
                    // `tables::is_dungeon_scene` check — so it's the one
                    // place the scene being left and the scene being
                    // entered could otherwise collide in the same roster.
                    // Any other destination (town, the open world) hands
                    // the roster back to issue #78's ordinary hold instead:
                    // no new preload rows can land there to collide with
                    // anything, and clearing eagerly would cut short the
                    // very feature issue #152 exists for — keeping a
                    // just-finished dungeon run's numbers on screen after
                    // zoning out to town.
                    //
                    // Gated on `!cut_short` too: a fight the `latch` above
                    // just froze has not had a single tick to be observed
                    // as `Ended` and recorded to history yet — resetting it
                    // in this same call would erase it before any external
                    // caller ever sees it, silently dropping the encounter.
                    // Deferring to the ordinary `NewFight` reset instead
                    // (fired by the new dungeon's first real hit) gives the
                    // publish loop the tick it needs first, exactly as
                    // issue #138's own `ServerChanged`-cut-short case
                    // already relies on. The tradeoff: a preload row for
                    // the new dungeon can still land next to this held
                    // fight's rows for the brief window until that first
                    // real hit — narrower than the bug this fixes (which
                    // had no such hit to eventually clean it up at all),
                    // but not eliminated by it.
                    //
                    // And gated on `self.last_known_scene_id` rather than
                    // the live `self.scene_id` (issue #295): `scene_id`
                    // itself goes `None` across a `ServerChanged`, since the
                    // destination really is unknown until this very packet
                    // — and every real zone transition in capture sends a
                    // `ServerChanged` first, so comparing against `scene_id`
                    // here always saw `None` in production and could never
                    // resolve immediately. `last_known_scene_id` is not
                    // cleared by `ServerChanged`, so by the time this packet
                    // lands the comparison is no longer ambiguous: a
                    // different id from the last one actually confirmed
                    // *is* a genuinely new instance, and the same id is the
                    // *late* confirmation of the instance the currently-held
                    // fight was already fought in
                    // (`a_scene_that_arrives_after_the_boss_still_captions_the_held_fight`,
                    // issue #152: `EnterScene` can land after the pull
                    // already started) — that case still falls back to
                    // `prune_stale_preloads` plus the ordinary `NewFight`
                    // reset, exactly as before #191.
                    // issue #202: a wipe latches `fight_end_ms` right away
                    // (see `apply_damage`'s `FightEndCause::Wipe` arm), so
                    // by the time this packet lands `cut_short` already
                    // reads false — it only ever catches a fight still
                    // `Active`. `wipe_hold` is that same unobserved-fight
                    // case under its own cause, so it has to hold off this
                    // reset for exactly the reason spelled out above: a
                    // wiped fight has not had a tick to be observed as
                    // `Ended` and recorded either, and clearing `players`
                    // here would drop it — death counts included — before
                    // anything ever sees it.
                    let entering_dungeon = tables::is_dungeon_scene(*level_map_id)
                        && self
                            .last_known_scene_id
                            .is_some_and(|id| id != *level_map_id);
                    if entering_dungeon && !cut_short && !self.wipe_hold {
                        self.reset(ResetReason::SceneChanged, self.last_event_ms);
                        // PR #198 review, finding 1: `reset` is shared with
                        // every in-instance reason (manual, `NewFight`,
                        // `BossHpRollback`), so it deliberately keeps the
                        // enemy map and only clears the per-fight flags on
                        // it. That is the wrong answer for a dungeon entry,
                        // which breaks entity identity as thoroughly as a
                        // reconnect does — so mirror the `ServerChanged`
                        // arm below and drop the old instance's entities
                        // outright. Left in place, issue #157's fallback in
                        // `recompute_boss` still ranks over them, and the
                        // previous run's never-killed boss — alive, and
                        // engaged well inside `BOSS_ENGAGEMENT_WINDOW_MS` —
                        // wins the new run's first hit on an unrecognized
                        // add, naming the wrong boss in the header, on the
                        // HP bar and in recorded history.
                        //
                        reason = Some(ResetReason::SceneChanged);
                    }

                    // Cleared *after* the reset, not before, so `reset_log`
                    // still reports the boss HP of the pull being cleared
                    // (PR #163 review, finding 3, for the same reason the
                    // `ServerChanged` arm latches before it clears) — and
                    // *outside* it (PR #205 review, finding 1), because the
                    // withheld cases above need the drop just as badly as
                    // the reset case does. A deferred reset would otherwise
                    // leave the departed instance's boss sitting in the map
                    // flagged `took_damage` and alive, and that flag is the
                    // whole candidate set `recompute_boss` ranks over: the
                    // new dungeon's very first `EnemyHp` packet would hand
                    // `boss_entity` to the boss of the dungeon just left, and
                    // issue #125's latch would then record it as *this*
                    // scene's final boss for the rest of the session.
                    //
                    // Safe for a frozen display, wipe hold included: what
                    // captions a held fight is `fight_identity` (issue
                    // #152), pinned while the fight was live, not the enemy
                    // map — and the hold's own re-engagement test wants the
                    // new instance's entities anyway, since the old
                    // instance's uids will never be seen again.
                    //
                    // `boss_entity` spelled out here, where the pre-#205 code
                    // could leave it to `reset`: the withheld paths run no
                    // `reset`, and so no `recompute_boss` to clear it.
                    // Mirrors the `ServerChanged` arm below.
                    //
                    // issue #139: a different dungeon/raid scene is a new
                    // instance with its own fresh objective sequence —
                    // carrying over the previous instance's
                    // `first_objective_id`/`objectives` would let the
                    // raid-boss reset detector (§6) false-trigger the
                    // moment this dungeon's own first objective happens to
                    // reuse a target id the old one also used. `Playing`
                    // (§2) is the authoritative "started" signal, but
                    // `Meter::in_dungeon_scene` gating rows on `scene_id`
                    // alone means this table needs to be just as clean for
                    // a dungeon whose `Playing` event this session never
                    // sees.
                    if entering_dungeon {
                        self.enemies.clear();
                        self.boss_entity = None;
                        // issue #316: `fight_end_boss_id` arms phase resume
                        // by looking a hit's target up in `enemies`, which
                        // this just emptied — leaving it set would have the
                        // new instance's first hit on anything read as
                        // "undecided" (`target_monster_id` unknown) and get
                        // withheld by `withholds_new_fight` until the window
                        // lapsed on its own, dropping every event in
                        // between. Mirrors the `ServerChanged` arm below.
                        self.fight_end_boss_id = None;
                        self.clear_dungeon_instance_state();
                    }

                    // issue #154 / PR #163 review, finding 1: a wipe hold
                    // belongs to the pull it froze, and that pull is in the
                    // scene being left. Carrying it out of the instance
                    // would leave the meter withholding every hit that is
                    // not on a recognized boss — out in the world, where
                    // there may never be one again.
                    //
                    // PR #205 review, finding 2: dropping it
                    // *unconditionally* undid the guard above in the very
                    // same call. Where the destination is another dungeon,
                    // the hold is precisely what withheld the reset, so
                    // clearing it here handed that deferred reset to the
                    // next ordinary trash hit — the run-back AoE issue #154
                    // exists to ignore — instead of to the boss
                    // re-engagement it requires. The invariant that
                    // actually holds: the hold survives a
                    // dungeon-to-dungeon transition and is dropped only on
                    // the way out to a non-dungeon destination. A dungeon
                    // destination whose reset *did* run has already had it
                    // cleared by `reset` itself.
                    if !tables::is_dungeon_scene(*level_map_id) {
                        self.wipe_hold = false;
                    }
                }
                self.scene_id = Some(*level_map_id);
                self.last_known_scene_id = Some(*level_map_id);
                reason
            }
            ProtocolEvent::ServerChanged { timestamp_ms } => {
                // issue #138: a server change (reconnect/zone transition)
                // only invalidates state keyed on identifiers that are
                // valid within one server session — uids are re-issued by
                // the new server, and the scene id is unknown until the
                // next `EnterScene`. It deliberately does **not** clear
                // `players`/totals itself: those are display state, and a
                // reconnect does not make them wrong — issue #152 relies on
                // exactly that to keep a held fight's numbers on screen
                // across a zone-out to town. Unlike the `Scene` arm above,
                // this event carries no destination scene id, so it cannot
                // tell a real dungeon-entry transition (issue #191) from a
                // same-instance reconnect; that call is left to the `Scene`
                // event that always follows once the destination is known.
                //
                // issue #12: a server change is as real a scene change as
                // any (the old scene's preloads can't possibly still be in
                // AOI range afterward), so mirror the Scene arm above and
                // drop preloaded rows nobody ever damaged — logging the
                // same summary line — while `scene_id` still names the
                // scene being left. Rows with real activity survive: they
                // are the display state this arm deliberately keeps.
                // Freeze the fight clock across the zoning gap, same as the
                // idle timeout does, so the held elapsed timer does not run
                // while the connection is down — and so `fight_end_ms`
                // being `Some` arms the `NewFight` path for the
                // reconnecting player's first real hit, or the `Scene` arm's
                // own `SceneChanged` reset if that hit lands in a dungeon.
                // A fight already held (or none running at all) is left
                // exactly as-is.
                //
                // Latched *before* the entity state is dropped below (PR
                // #163 review, finding 3): `latch_fight_end` logs the boss
                // identity, and it reads it out of `boss_entity`/`enemies`, so
                // clearing those first made this diagnostic always say
                // `boss_monster_id=<unknown>` — losing the one fact it
                // exists to record about a fight cut short by a reconnect.
                if self.fight_start_ms.is_some() && self.fight_end_ms.is_none() {
                    self.latch_fight_end(
                        FightEndCause::ServerChanged,
                        *timestamp_ms,
                        *timestamp_ms,
                        self.boss_monster_id(),
                    );
                }

                self.prune_stale_preloads();
                self.enemies.clear();
                self.boss_entity = None;
                // issue #316: same reason as the `Scene` arm's
                // `entering_dungeon` clear above — a reconnect empties
                // `enemies`, and a stale `fight_end_boss_id` armed against
                // it would withhold the reconnecting player's first hit
                // (and every one after it) as "undecided" until the resume
                // window expired on its own, instead of starting the next
                // fight immediately.
                self.fight_end_boss_id = None;
                // issue #139: as invalid across a reconnect as
                // `enemies`/`boss_entity` — the new server session may not
                // even land back in the same dungeon, let alone the same
                // objective sequence, so nothing about the old instance's
                // dungeon-state tracking can be trusted to describe it.
                self.clear_dungeon_instance_state();
                if let Some(msg) = scene_transition_log(self.scene_id, None) {
                    log::info!("{msg}");
                }
                self.scene_id = None;

                // issue #154: the wipe hold's re-engagement test reads the
                // enemy map that was just cleared, so it can no longer
                // recognize anything. Leaving the instance hands the hold
                // back to issue #78's ordinary rule, where the next real
                // hit clears it.
                self.wipe_hold = false;

                None
            }
            // issue #139 slice 2: the dungeon-state / raid-boss-reset /
            // objective-gated-fight-end behaviour spec "Meter behaviour"
            // §§1-8. Every path below is reachable only through one of
            // these three events, so a session that never sees `0x17`/
            // `0x18` never calls any of them and behaves bit-identically
            // to before this slice.
            ProtocolEvent::DungeonState { state, .. } => self.apply_dungeon_state(*state),
            ProtocolEvent::DungeonObjective {
                target_id,
                nums,
                complete,
            } => self.apply_dungeon_objective(*target_id, *nums, *complete),
            ProtocolEvent::DungeonObjectiveRemoved { target_id } => {
                self.apply_dungeon_objective_removed(*target_id);
                None
            }
            // issue #215: an entity left AOI. Almost always nothing to do —
            // see `apply_enemy_gone` for the cases where it is allowed to
            // stand in for a death signal that never arrived, including the
            // server's own `DisappearReason::Dead` (issue #276).
            ProtocolEvent::EnemyGone {
                entity,
                uid,
                reason,
            } => {
                self.apply_enemy_gone(entity.or_display(*uid, EntityKind::Monster), *uid, *reason);
                None
            }
            ProtocolEvent::DungeonVar { name, value } => {
                // §7: `IsFinishTarget` with a non-zero value is ZDPS's
                // documented completion fallback (never observed in this
                // build's captures, same caveat as `Playing` — see
                // `ResetReason::DungeonStarted`), latched exactly like §3's
                // `End`/`Settlement`. Every other var name is real,
                // decoded, and deliberately ignored — the meter has no use
                // for `music_value`, `cur_qinshi`, etc.
                if name == "IsFinishTarget"
                    && *value != 0
                    && self.fight_start_ms.is_some()
                    && self.fight_end_ms.is_none()
                {
                    self.latch_fight_end(
                        FightEndCause::DungeonEnded,
                        self.last_event_ms,
                        self.last_event_ms,
                        self.boss_monster_id(),
                    );
                }
                None
            }
        }
    }

    /// Drops everything the meter tracks about *one dungeon instance*
    /// (issue #139): the flow state, the objective table, and both
    /// objective-id markers. Every path that leaves an instance behind
    /// clears all four together — a scene change into a different
    /// dungeon, a server change, the dungeon reporting itself back to
    /// `Null`, and a `Playing` re-send restarting the run in place — so
    /// they are named in exactly one spot rather than four (PR #226
    /// review, finding 4). Clearing only some of them is precisely what
    /// makes §6's raid-boss detector fire on the *previous* instance's
    /// ids.
    ///
    /// Deliberately not `Meter::reset`'s job: a reset is about the
    /// encounter (rows, enemies, fight clocks), and plenty of resets
    /// happen *within* one instance, where this tracking must survive.
    fn clear_dungeon_instance_state(&mut self) {
        self.dungeon_state = None;
        self.objectives.clear();
        self.first_objective_id = None;
        self.current_objective_id = None;
    }

    /// Applies a `DungeonState` transition (issue #139 §§2-4).
    fn apply_dungeon_state(&mut self, state: EDungeonState) -> Option<ResetReason> {
        self.dungeon_state = Some(state);
        match state {
            // §2: `Playing` is the authoritative "a dungeon run just
            // started" signal — force a fresh encounter even though
            // nothing else (damage, a scene change) has necessarily
            // happened yet.
            //
            // PR #226 review, finding 1: the reset alone is not enough.
            // `reset` clears the encounter and nothing else — the
            // instance-level objective tracking is not its business (see
            // `clear_dungeon_instance_state`) — and a raid retried in
            // place re-sends `Playing` with no `Scene`/`ServerChanged`/
            // `Null` in between (`ResetReason::DungeonStarted`: "inside
            // the same instance either way"). Without the clear, the new
            // attempt inherits the previous one's `first_objective_id`
            // and objective table, and §6 arms on stale data. The flow
            // state is re-asserted straight after, since the helper drops
            // that too and `Playing` is exactly what this event reported.
            EDungeonState::Playing => {
                self.clear_dungeon_instance_state();
                self.dungeon_state = Some(state);
                self.reset(ResetReason::DungeonStarted, self.last_event_ms);
                Some(ResetReason::DungeonStarted)
            }
            // §3: the dungeon telling the meter its own fight is over is
            // more authoritative than any heuristic — latch it under its
            // own cause so #151's fight-end diagnostic can tell it apart
            // from a boss death or an idle timeout. Timestamped by
            // `last_event_ms`, not "now": the fight ended when the hitting
            // stopped, the same rule the `Scene` arm's `SceneChanged` latch
            // above already follows.
            EDungeonState::End | EDungeonState::Settlement => {
                if self.fight_start_ms.is_some() && self.fight_end_ms.is_none() {
                    self.latch_fight_end(
                        FightEndCause::DungeonEnded,
                        self.last_event_ms,
                        self.last_event_ms,
                        self.boss_monster_id(),
                    );
                }
                None
            }
            // §4: back to open world (or a dungeon the meter has not
            // confirmed is even running). Nothing about the previous
            // instance's objective progression is valid for whatever comes
            // next, so drop it all and fall back to the heuristics-only
            // path — the same tracking a session that never sees a
            // dungeon packet never populates in the first place.
            EDungeonState::Null => {
                self.clear_dungeon_instance_state();
                None
            }
            EDungeonState::Active
            | EDungeonState::Ready
            | EDungeonState::Vote
            | EDungeonState::Unknown(_) => None,
        }
    }

    /// Applies one `DungeonObjective` update (issue #139 §§1,5,6): records
    /// the latest known `nums`/`complete` for `target_id` in
    /// `self.objectives`, and — when the event carries the wire's "new
    /// objective" signature (a different id than the one currently
    /// running, freshly at zero progress and not yet complete) — advances
    /// `current_objective_id` and checks for the raid-boss reset.
    fn apply_dungeon_objective(
        &mut self,
        target_id: i32,
        nums: Option<i32>,
        complete: Option<bool>,
    ) -> Option<ResetReason> {
        let entry = self.objectives.entry(target_id).or_default();
        // Partial-update semantics (spec "Wire format": each of
        // `TargetData`'s fields is independently optional, and an update
        // entry commonly omits some of them) — only overwrite a field this
        // event actually carried, so an update that only touches
        // `complete` doesn't clobber an already-known `nums` back to
        // unknown.
        if let Some(n) = nums {
            entry.nums = Some(n);
        }
        if let Some(c) = complete {
            entry.complete = Some(c);
        }

        // §6 arms on `first_objective_id`, so the first objective the
        // instance reports has to establish it — *whatever* state that
        // objective is reported in (PR #226 review, finding 3). A target
        // that arrives already complete fails the new-objective signature
        // below and used to return early, leaving `first_objective_id`
        // unset for the rest of the instance: §6 then never armed at all,
        // and the next genuine transition wrongly read as "first".
        // Deliberately separate from `current_objective_id`, which only a
        // real transition may move — an already-complete objective is not
        // running.
        if self.first_objective_id.is_none() {
            self.first_objective_id = Some(target_id);
        }

        let prior_current = self.current_objective_id;
        // §5: the wire's signature for "a new objective just started" — a
        // different target id, freshly at zero progress and not yet
        // complete. An update to the objective already current (progress
        // ticking up, or its own completion) never matches `target_id !=
        // prior_current`, so it is recorded above but changes nothing else
        // — it is not a transition, just progress on the current phase.
        let is_new_objective =
            Some(target_id) != prior_current && complete == Some(false) && nums == Some(0);
        if !is_new_objective {
            return None;
        }

        // §6 (issue #210's case): the raid-boss reset detector. Fires only
        // once the instance has genuinely moved off the first objective —
        // `prior_current` is known (`Some`) and differs from
        // `first_objective_id` — and the *new* current objective is that
        // same first id reappearing: "one raid boss killed while others
        // are unbeaten". The very first objective this instance ever
        // reports also technically "equals first_objective_id" (this same
        // call is what just established it), but `prior_current` is
        // `None` at that point, so `prior_current.is_some()` alone keeps
        // it from false-triggering on an instance's opening objective.
        let raid_boss_reset = prior_current.is_some()
            && prior_current != self.first_objective_id
            && Some(target_id) == self.first_objective_id;

        self.current_objective_id = Some(target_id);

        if raid_boss_reset {
            self.reset(ResetReason::DungeonStarted, self.last_event_ms);
            Some(ResetReason::DungeonStarted)
        } else {
            None
        }
    }

    /// Applies one objective *removal* (issue #139; PR #226 review,
    /// finding 2): the wire dropped `target_id` out of its objective
    /// hashmap without ever reporting it complete. A removal is not a
    /// completion, so the entry is dropped rather than marked done — and
    /// when it was the objective §8 is gating on, `current_objective_id`
    /// goes back to unknown, which is what lifts that gate. Left standing,
    /// a removed-but-never-completed objective held
    /// `dungeon_objective_still_running` true for the rest of the
    /// instance, suppressing every boss-death fight end in it. An
    /// objective the meter never tracked is a no-op.
    ///
    /// `first_objective_id` deliberately survives: it records which
    /// objective *opened* this instance, which a later removal does not
    /// change, and §6 needs it for the whole run.
    fn apply_dungeon_objective_removed(&mut self, target_id: i32) {
        self.objectives.remove(&target_id);
        if self.current_objective_id == Some(target_id) {
            self.current_objective_id = None;
        }
    }

    fn apply_damage(&mut self, d: &DamageEvent) -> Option<ResetReason> {
        // issue #78: pin the end *before* this event touches the encounter's
        // clocks — a monster's swing at a player extends `last_event_ms`
        // without ever producing a row, which would otherwise drag an
        // already-ended fight back into `Active`.
        if let Some(end_ms) = self.fight_ended_at(d.timestamp_ms) {
            self.latch_fight_end(
                FightEndCause::IdleTimeout,
                end_ms,
                d.timestamp_ms,
                self.boss_monster_id(),
            );
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
            self.fight_end_observed_ms = None;
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
        //
        // ...and `damage_in_post_end_grace`, a third, narrower exception: a
        // hit landing inside `FightConfig::post_end_grace_ms` of the end,
        // on a target this fight had already damaged, is not withheld
        // pending some other signal, like the two above — it is simply too
        // soon after the end, and on a target too well established as this
        // fight's own, to be evidence of a *new* fight rather than the tail
        // of the one that just finished (the reference implementation's
        // rationale; see `FightConfig::post_end_grace_ms`). Must not
        // re-open or extend the held fight either, so this sits in the same
        // guard as the withholds, not as a fallback after it.
        let mut reason = None;
        if self.fight_end_ms.is_some()
            && d.attacker_kind == EntityKind::Player
            && !d.is_heal
            && !self.withholds_new_fight(d)
            && !self.withholds_after_wipe(d)
            && !self.damage_in_post_end_grace(d)
        {
            self.reset(ResetReason::NewFight, d.timestamp_ms);
            reason = Some(ResetReason::NewFight);
        }

        // Still held (the reset above clears `fight_end_ms`, so this can only
        // be the "combat the user isn't part of", withheld, or post-end
        // grace case): the displayed fight stays frozen — `fight_end_ms`,
        // the elapsed timer and `FightState::Ended` are all left exactly as
        // they are — but a grace-window event still gets folded into the
        // ended fight's *stats* rather than being dropped outright (issue
        // #post-end-grace; see `FightConfig::post_end_grace_ms`).
        // `apply_damage_grace` is deliberately narrower than the rest of
        // this function: it skips the enemy/boss/wipe bookkeeping below so
        // a grace-window event can never change what a later, genuinely new
        // hit's `resumes_held_fight`/`withholds_after_wipe` checks see —
        // those must keep reading exactly the inputs they did before this
        // window existed.
        if self.fight_end_ms.is_some() {
            if self.damage_in_post_end_grace(d) {
                return self.apply_damage_grace(d);
            }
            return None;
        }

        // issue #212: a player *acting* is the only evidence this crate
        // ever gets that they are back up — `event::PlayerInfo` carries no
        // HP, and no other `apply_*` entry point sees a player-side signal
        // at all. So any outgoing event counts, heal-typed ones included: a
        // healer or support whose whole output is heals would otherwise
        // stay `alive: false` from their first death to the end of the
        // pull, and `party_is_wiped` would read the party as down the next
        // time everyone else happened to be between a death and their next
        // hit (PR #224 review, finding 2).
        //
        // Above the death handling below, so that when a single event is
        // both — a player landing a killing blow on themselves, via a
        // reflect or a self-damaging skill — the death is the write that
        // lands last and they stay down (finding 1). `set_alive` lets the
        // equal timestamps through in that order deliberately.
        //
        // `get_mut`, not the `entry` API that the attacker path below
        // uses: a heal is proof of life for a row the roster already
        // holds, but it must not *create* one, or a stranger healing their
        // way past the player in town would open a row in a damage meter
        // they are no part of.
        if d.attacker_kind == EntityKind::Player
            && let Some(stats) = self.players.get_mut(&attacker_key(d))
        {
            stats.set_alive(true, d.timestamp_ms);
        }

        // `d.is_dead` flags that `target_uid` (the victim, not the
        // attacker) died from this hit — count it against the target
        // regardless of who or what dealt the blow (issue #49), and
        // regardless of whether the killing packet is heal-typed (e.g. a
        // negative/lethal heal). This must run before the `is_heal` early
        // return below so heal-typed death packets still record deaths.
        if d.is_dead && d.target_kind == EntityKind::Player {
            self.record_death(target_key(d), d.target_uid, d.timestamp_ms);
            // issue #154: that death may have been the last one standing.
            // A wipe is a fight *end* — the moment a damage meter is most
            // useful — so latch the hold here instead of leaving the
            // attempt to be destroyed by the HP-rollback heuristic when the
            // boss's bar refills a second later.
            //
            // Gated on `engaged_boss_still_up` (PR #163 review, finding 1):
            // the hold is only ever *lifted* by a hit on a recognized boss,
            // so it may only be *entered* where there is one to re-engage —
            // a damaged, living, recognized boss inside an instance, which
            // is the wipe issue #154 is about. Without that gate a solo
            // player dying once to a field mob satisfied `party_is_wiped`
            // and froze the meter until they zoned or reset by hand, with
            // every hit, death and point of damage in between silently
            // dropped. Elsewhere a wipe still freezes the numbers — the
            // idle timeout takes it, now that issue #155 stops the mob
            // swinging at the corpses from holding the fight open.
            //
            // The `in_dungeon_scene` half is spelled out *here* rather than
            // inherited from `engaged_boss_still_up`, which used to carry it
            // (issue #313 removed it there): the idle path's reason for the
            // scene check was vestigial, this one's is not. Scene 7152 picks
            // the wipe hold up regardless, via issue #313's
            // `DUNGEON_SCENE_IDS` addition.
            if self.party_is_wiped()
                && self.in_dungeon_scene()
                && self.engaged_boss_still_up(d.timestamp_ms)
            {
                self.latch_fight_end(
                    FightEndCause::Wipe,
                    d.timestamp_ms,
                    d.timestamp_ms,
                    self.boss_monster_id(),
                );
                self.wipe_hold = true;
            }
        }

        // issue #245: the per-tab breakdowns the skill window's Heal,
        // "Skill dealt" and "Skill received" tabs read. Recorded here, and
        // deliberately above both of the early returns that follow: the
        // `is_heal` return below drops the events the Heal tab is entirely
        // about, and the `attacker_kind != Player` return further down
        // drops the monster damage the received tab is mostly about.
        self.record_breakdowns(d);

        // Healing never touches damage totals or fight timing — it reaches
        // the UI only through the Heal / dealt tabs recorded just above.
        if d.is_heal {
            return reason;
        }

        if d.target_kind == EntityKind::Monster {
            let enemy = self.enemies.entry(target_key(d)).or_default();
            enemy.took_damage = true;
            // The same fact minus the reset, and with a clock on it (PR
            // #163 review, finding 2): `recompute_boss`'s issue #157
            // fallback has to tell a boss this party is fighting *now*
            // from one that has only ever stood in AOI range, and from one
            // the party fought an hour ago and abandoned — and
            // `took_damage` cannot answer either question in the window
            // right after a reset clears it. `max` keeps it monotonic, so
            // an out-of-order packet cannot make the engagement look older
            // than it is.
            enemy.last_damaged_ms = enemy.last_damaged_ms.max(Some(d.timestamp_ms));
            // issue #124: remember that this one died, and in what order, so
            // the "is any other boss in this encounter still alive?" question
            // below has an answer even when no HP sync ever reports the
            // corpse at 0 — and so `recompute_boss` can keep the header on
            // the phase that just fell. Must run before `recompute_boss`,
            // which reads both.
            if d.is_dead {
                self.mark_enemy_dead(target_key(d));
            }
            // issue #210/#211: captured *before* `recompute_boss`, which
            // ranks a living recognized boss above a dead one — so the
            // instant `mark_enemy_dead` above stamps this uid's death
            // order, any other recognized boss already damaged and still
            // alive (the raid's next selection, say) outranks the corpse
            // and `recompute_boss` moves `boss_entity` off it. Reading
            // `boss_entity` after that point can no longer tell whether the
            // enemy that just died was the one being tracked.
            let was_tracked_boss = self.boss_entity == Some(target_key(d));
            self.recompute_boss();
            // issue #78: a recognized boss dying ends the fight now, rather
            // than after the idle timeout, so the meter freezes on the kill
            // instead of on a straggler's last tick of DoT damage.
            if self.fight_cfg.end_on_boss_death && d.is_dead && was_tracked_boss {
                self.end_fight_on_boss_death(target_key(d), d.timestamp_ms);
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

        self.accumulate_damage_stats(d);

        reason
    }

    /// The narrow, stats-only path a player's damage/heal event takes when
    /// it lands inside the post-end grace window of an already-ended fight
    /// (`FightConfig::post_end_grace_ms`; see `apply_damage`'s "still held"
    /// branch, which is this method's only caller, and
    /// `Self::damage_in_post_end_grace` for what already screened `d` in —
    /// a heal, a monster's own damage, or a player's real hit on a target
    /// this fight had already engaged, never an unrelated add or the next
    /// pull's opener).
    ///
    /// Deliberately does far less than the live path above:
    ///
    /// * `record_breakdowns` and (for a non-heal hit) the same total/hit/
    ///   crit/lucky accumulation `apply_damage` itself does — the whole
    ///   point of the grace window, so a straggling DoT tick or a killing-
    ///   blow retransmit still counts.
    /// * Nothing else. `last_event_ms`, `fight_start_ms` and `fight_end_ms`
    ///   are all untouched, so the frozen elapsed timer and the DPS
    ///   denominator (`snapshot`'s `dps_duration_ms`, which reads
    ///   `last_event_ms`) can never move because of a grace-window event.
    ///   And the enemy/boss bookkeeping (`took_damage`, `recompute_boss`,
    ///   `end_fight_on_boss_death`, the wipe latch) is skipped outright:
    ///   `end_fight_on_boss_death` already no-ops once `fight_end_ms` is
    ///   `Some`, but the wipe latch's `self.wipe_hold = true` does not, and
    ///   letting a grace-window player death set it would change what
    ///   `withholds_after_wipe` sees on the *next* genuinely new fight —
    ///   exactly the "same inputs" invariant this window must not disturb.
    fn apply_damage_grace(&mut self, d: &DamageEvent) -> Option<ResetReason> {
        self.record_breakdowns(d);
        // Same gate `apply_damage` reaches this accumulation through: only
        // a player's own non-heal damage feeds player/skill totals. Without
        // it a monster's swing landing in the grace window (attacker_kind
        // `Monster`) would open a row keyed on the monster's uid.
        if !d.is_heal && d.attacker_kind == EntityKind::Player {
            self.accumulate_damage_stats(d);
        }
        None
    }

    /// The per-player/per-skill hit, total-damage, crit and lucky
    /// accumulation shared by the live path in `apply_damage` and the
    /// post-end grace path in `apply_damage_grace`. Callers are responsible
    /// for everything this does *not* do: starting the fight clock, gating
    /// on `attacker_kind`/`is_heal`, and touching `last_event_ms` — none of
    /// which the grace path may do (see its doc comment).
    fn accumulate_damage_stats(&mut self, d: &DamageEvent) {
        let cached = self.name_lookup(d.attacker_uid);
        let stats = self
            .players
            .entry(attacker_key(d))
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
        // Per-skill `hits` is bumped outside the `!d.is_miss` guard below so
        // it stays definitionally identical to the player-level `hits` above
        // — a miss is a swing on some skill, not a non-event.
        let skill = stats.skills.entry(d.skill_id).or_default();
        skill.hits += 1;
        if !d.is_miss {
            stats.total_damage += d.value;
            skill.total_damage += d.value;
            if d.crit {
                stats.crit_hits += 1;
                stats.crit_damage += d.value;
                skill.crit_hits += 1;
                skill.crit_damage += d.value;
                skill.max_crit = skill.max_crit.max(d.value);
            }
            if d.lucky {
                stats.lucky_hits += 1;
                stats.lucky_damage += d.value;
                skill.lucky_hits += 1;
                skill.lucky_damage += d.value;
            }
        }
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
    /// cannons). Suppressing costs only the instant freeze — the idle
    /// timeout still ends the fight.
    ///
    /// issue #210/#211: *not* a raid boss pulled alongside another that the
    /// party has genuinely moved on from, though — `other_living_boss`
    /// is scene-aware. In a boss-select scene a sequential next selection
    /// (untouched, or touched long enough ago to fall outside
    /// `BOSS_ENGAGEMENT_WINDOW_MS`) does not count, so killing the
    /// in-progress selection still ends the fight. A boss genuinely being
    /// fought *concurrently* with the one that just died — Dreambloom
    /// Ruins' Caprahorn pair, which spawns inside a boss-select scene same
    /// as the sequential raids do — still counts, exactly as it does
    /// outside a boss-select scene.
    fn end_fight_on_boss_death(&mut self, entity: EntityId, now_ms: u64) {
        // The display number, for the diagnostics below only — `entity` is
        // what indexes `enemies` (issue #335).
        let uid = entity.display_uid();
        // issue #210/#211: `uid`'s own monster id, not `self.boss_monster_id()`
        // (== whatever `self.boss_entity` currently is). Both call sites used to
        // guard on `self.boss_entity == Some(uid)`, so the two were always the
        // same id — but that guard is now the pre-`recompute_boss` capture
        // `was_tracked_boss` (defect 2), and by the time this runs
        // `recompute_boss` may already have moved `boss_entity` onto another
        // living boss `uid`'s death just promoted (e.g. a raid's next
        // selection). This function must still record *this* dying boss's
        // identity, not whichever one the header now follows.
        let monster_id = self.enemies.get(&entity).and_then(|e| e.monster_id);
        let recognized = monster_id.is_some_and(tables::is_boss_monster);
        // Guarded on an in-progress fight so a kill packet arriving while no
        // fight is running (the tail of a pull the user just reset away)
        // can't leave a stale end time latched for the *next* fight to trip
        // over.
        // issue #139 §8 (issue #210's case): while the dungeon's own
        // objective tracking says the instance is still running and its
        // current objective is not yet complete, this boss's death is a
        // phase of the instance, not the end of the fight —
        // `other_living_boss` cannot catch this on its own, since it
        // only sees enemies the party has actually `took_damage` on, and a
        // raid's next boss standing unengaged nearby is invisible to it
        // until the party's first hit lands.
        if !recognized || self.fight_start_ms.is_none() || self.fight_end_ms.is_some() {
            return;
        }
        let other_boss = self.other_living_boss(entity, now_ms);
        // issue #256: computed unconditionally rather than short-circuited
        // behind `other_boss.is_none()` — the diagnostic log line below
        // reports `dungeon_objective_still_running={objective_holds}` on
        // every path that falls through here, including when `other_boss`
        // alone is why the fight didn't end, so the value is needed either
        // way. Don't restore the `&&`'s short-circuit; it would silently
        // break that field.
        let objective_holds = self.dungeon_objective_still_running();
        if other_boss.is_none() && !objective_holds {
            self.latch_fight_end(FightEndCause::BossDeath, now_ms, now_ms, monster_id);
            self.fight_end_boss_id = monster_id;
            return;
        }
        // issue #256: the two guards above are the *only* way a recognized
        // boss's death fails to end a running fight, and until now a refusal
        // was invisible — the fight simply fell through to the idle timeout a
        // minute later and the log said `cause=idle_timeout`, with nothing to
        // say which guard had dropped the signal or on what. Sparse by
        // construction (issue #69): a recognized boss dies at most a handful
        // of times per instance, so this is one line per boss death that did
        // not end its fight, never a per-packet flood. Both guards and every
        // input either of them read are named, so one capture is enough to
        // decide the next case without another round of guessing.
        log::info!(
            "encounter: boss death of uid={uid} monster_id={} did not end the fight: \
             other_living_boss={} dungeon_objective_still_running={objective_holds} \
             scene={} boss_select={} dungeon_state={:?} current_objective={:?} \
             objective_complete={:?} (issue #256)",
            monster_id.map_or(-1i64, i64::from),
            other_boss.map_or(-1, EntityId::display_uid),
            self.scene_id.map_or(-1i64, i64::from),
            self.scene_id.is_some_and(phase::is_boss_select_scene),
            self.dungeon_state,
            self.current_objective_id,
            self.current_objective_id
                .and_then(|id| self.objectives.get(&id))
                .and_then(|obj| obj.complete),
        );
    }

    /// True while the dungeon's own tracking says a boss death alone must
    /// not end the fight (issue #139 §8): the instance is confirmed in
    /// progress (`dungeon_state` is `Some` and not `Null`) and the current
    /// objective is known and not yet complete. `false` whenever no
    /// dungeon event has ever been seen (`dungeon_state` is `None`), so a
    /// session on a build that never sends `0x17`/`0x18` never gates
    /// `end_fight_on_boss_death` here at all.
    fn dungeon_objective_still_running(&self) -> bool {
        // issue #256: never inside a boss-select raid. §8's premise is that
        // a boss dying while the instance's own objective is unfinished is a
        // *phase* of one fight rather than the end of it — true of an
        // ordinary dungeon, where the objective advances with the run, and
        // false of a raid, where the objective tracks the whole raid ("defeat
        // the Remnants") and stays incomplete across every selection's death
        // but the last. Gating there suppressed **every** boss-death end in
        // scene 13023 — six days of logs hold zero `cause=boss_death` for any
        // 103xxx boss — and reinstated, from the dungeon side, exactly the
        // hold that issue #210/#211 had just removed from
        // `other_living_boss`: a raid's next selection standing unengaged
        // must not keep the current selection's death from ending the fight.
        // The unengaged-neighbour case §8 was reaching for is therefore
        // conceded here on purpose; killing a selection *is* the end of a
        // pull, and a genuinely concurrent pair (Dreambloom Ruins' Caprahorn
        // twins) is still held open by `other_living_boss`'s co-engagement
        // rule, which sees them because the party is hitting both.
        if self.scene_id.is_some_and(phase::is_boss_select_scene) {
            return false;
        }
        if !self.dungeon_state.is_some_and(|s| s != EDungeonState::Null) {
            return false;
        }
        self.current_objective_id
            .and_then(|id| self.objectives.get(&id))
            .is_some_and(|obj| obj.complete != Some(true))
    }

    /// Latches the fight end at `end_ms` and logs the single `info`-level
    /// line that says a fight ended and why (issue #151's diagnostics gap).
    /// `boss_monster_id` is the id the log line should name — ordinarily
    /// `self.boss_monster_id()`, except from `end_fight_on_boss_death`
    /// (issue #210/#211), which passes the dying boss's own id since
    /// `self.boss_entity` may already have moved on by the time this runs.
    ///
    /// `observed_ms` is when this call is actually happening — `now_ms` for
    /// `tick`/`end_fight_on_boss_death`, the current event's `timestamp_ms`
    /// everywhere else — recorded into `fight_end_observed_ms` (issue #316).
    /// For every cause but `IdleTimeout` this equals `end_ms`, since those
    /// ends are latched the instant they happen; an idle-timeout end's
    /// `end_ms` is the last *player* hit, which can be
    /// `BOSS_ENGAGEMENT_WINDOW_MS` in the past by the time
    /// `engaged_boss_still_up` lets the timeout through, so the two diverge
    /// there specifically and `armed_phase_hold` anchors on this field
    /// instead.
    ///
    /// Every path that ends a fight goes through here — boss death, idle
    /// timeout, party wipe, server change — so the line fires exactly once
    /// per fight end: a fight already latched returns untouched, which is
    /// also what makes the repeated "pin the end" calls in `apply_damage`
    /// and `tick` idempotent. That same guard is why an `IdleTimeout` end
    /// arming `fight_end_boss_id` below is safe to do unconditionally here
    /// rather than at each of its two call sites: it only ever runs on the
    /// call that performs the actual latch.
    fn latch_fight_end(
        &mut self,
        cause: FightEndCause,
        end_ms: u64,
        observed_ms: u64,
        boss_monster_id: Option<u32>,
    ) {
        if self.fight_end_ms.is_some() {
            return;
        }
        self.fight_end_ms = Some(end_ms);
        self.fight_end_observed_ms = Some(observed_ms);
        // issue #316: arm phase resumption on an idle-timeout end too, not
        // only a boss death. `end_fight_on_boss_death` names the dying
        // boss's own uid because `boss_entity` may already have moved on by the
        // time it runs; nothing has moved on here, so the currently engaged
        // recognized boss is the right (and only sensible) answer.
        if cause == FightEndCause::IdleTimeout {
            self.fight_end_boss_id = self.engaged_boss_monster_id();
        }
        log::info!("{}", fight_end_log(cause, boss_monster_id));
    }

    /// The monster id of the currently selected boss target, if it has one.
    fn boss_monster_id(&self) -> Option<u32> {
        self.boss_entity
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.monster_id)
    }

    /// The monster id of a recognized boss this fight has damaged and that
    /// is not known to be dead (issue #316) — `Meter::is_damaged_living_boss`
    /// without the recency half `engaged_boss_still_up` requires.
    ///
    /// Used only to arm phase resumption on an idle-timeout end: by the time
    /// that end is reachable at all, `engaged_boss_still_up` has already
    /// gone false, which means no enemy currently satisfies "engaged within
    /// `BOSS_ENGAGEMENT_WINDOW_MS`" — the very condition that just released
    /// the timeout. Asking that same recency-bounded question here would
    /// therefore always answer `None`. What this fight ended *on* is still a
    /// meaningful identity to resume against, so this asks the
    /// recency-free half of the question instead: is there a recognized
    /// boss the party actually fought this encounter and never saw die.
    ///
    /// "Any", not "`boss_entity`'s own", for the same reason
    /// `engaged_boss_still_up` is: a pull can have two bosses up at once,
    /// and the header's current selection is not necessarily the one whose
    /// idling out just ended the fight.
    ///
    /// `self.enemies` is a `HashMap`, so when more than one candidate
    /// qualifies the iteration order is not meaningful and must not decide
    /// the answer. Ties are broken deterministically: a candidate with a
    /// curated [`phase::has_phase_group`] wins first (that is the one this
    /// helper exists to resume against), then the one damaged most
    /// recently, then — to make the choice fully deterministic even between
    /// two otherwise-identical candidates — the lowest uid.
    fn engaged_boss_monster_id(&self) -> Option<u32> {
        self.enemies
            .iter()
            .filter(|(_, e)| is_damaged_living_boss(e))
            .filter_map(|(&uid, e)| e.monster_id.map(|monster_id| (uid, monster_id, e)))
            .max_by_key(|(uid, monster_id, e)| {
                (
                    phase::has_phase_group(*monster_id),
                    e.last_damaged_ms,
                    std::cmp::Reverse(*uid),
                )
            })
            .map(|(_, monster_id, _)| monster_id)
    }

    /// Records that `uid` has died, assigning it the next rank in this
    /// encounter's death order (issue #124). Idempotent: the first signal
    /// wins, so a death packet followed by the corpse's HP sync to 0 (or a
    /// retransmit of either) does not re-stamp the rank and reshuffle
    /// `recompute_boss`'s view of who fell last.
    fn mark_enemy_dead(&mut self, entity: EntityId) {
        let next = self.deaths_seen + 1;
        let assigned = match self.enemies.get_mut(&entity) {
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

    /// Reacts to `uid` leaving the client's area of interest (issue #215).
    ///
    /// A despawn is not a death signal, and this function's default answer is
    /// to do nothing at all. What it *can* have is the server's own reason
    /// for the despawn (`reason`, issue #276): `pb::DisappearEntity`'s
    /// optional tag 2, decoded into [`DisappearReason`]. Only
    /// [`DisappearReason::Dead`] states a death; `Destroy` (an eviction),
    /// `TransferLeave` (a zone-out), `Normal` (ordinary streaming churn) and
    /// an unrecognized future value all state the opposite or nothing. And
    /// tag 2 is genuinely optional — 382 of 851 captured disappear entries,
    /// 23 of them monsters, carry none — so a `None` leaves this exactly
    /// where issue #215 left it: inferring from health and engagement.
    ///
    /// It exists because the meter's two real death signals can both go
    /// missing. Issue #210: across every logged clear of scene 13023 the
    /// first boss's death produced neither a `DamageEvent::is_dead` nor an
    /// `EnemyHp` syncing `curr_hp` to 0 — the boss simply stopped being
    /// mentioned. `EnemyState::is_alive` reads a never-observed death as
    /// alive forever, which used to wedge the fight open permanently; issue
    /// #210/#211 bounded that wedge to `BOSS_ENGAGEMENT_WINDOW_MS` but the
    /// end still lands as `cause=idle_timeout` a minute late, with the next
    /// boss's opening damage already accumulated into the dead one's rows.
    /// The despawn is the one packet that *does* arrive in that case, and
    /// under a tight enough rule it recovers the missing death.
    ///
    /// ## The rule, and why every clause of it is load-bearing
    ///
    /// A despawn is read as a death only when **all** of the following hold.
    /// Each one alone admits far too much; the conjunction is what makes this
    /// safe enough to ship:
    ///
    /// 1. `end_on_boss_death` is on — the same switch every other
    ///    boss-death-driven end already respects.
    /// 2. A fight is running and not already ended. A despawn can never
    ///    *start* anything, and must never re-stamp a held fight's end time.
    /// 3. The enemy is the currently tracked boss (`boss_entity`). This is the
    ///    strongest clause: the header is following it, so `recompute_boss`
    ///    has already ranked it above everything else the party has engaged.
    ///    A sibling boss, an add, or a mob nobody is looking at cannot end a
    ///    fight by vanishing, however low its health.
    /// 4. It took damage from the party. A boss standing in a room the party
    ///    walked past and streamed out again was never part of an encounter —
    ///    the same scoping `other_living_boss` and `engaged_boss_still_up`
    ///    use.
    /// 5. It is a *recognized* boss (`tables::is_boss_monster`). Without this
    ///    the biggest trash mob in a pull ends the fight every time the AOI
    ///    evicts it, exactly as it would in `end_fight_on_boss_death`.
    /// 6. It is not already known to be dead — so the ordinary case (a real
    ///    death signal, then the corpse's despawn seconds later) stays inert
    ///    rather than stamping a second death rank.
    /// 7. The party was hitting it within `BOSS_ENGAGEMENT_WINDOW_MS` of the
    ///    last combat event. A boss burned low, abandoned, and evicted from
    ///    AOI minutes later is a range-out; a corpse removed mid-pull is not.
    /// 8. The despawn is plausibly a death. Clauses 1-7 are all about
    ///    *which* enemy vanished; this is the only one that speaks to
    ///    whether it *died*, and since issue #276 it is answered in one of
    ///    three ways:
    ///
    ///    - **`reason == Some(Dead)`** — the server said so. Sufficient on
    ///      its own: no health threshold is consulted, which is the whole
    ///      point. The threshold is a proxy — correctly *sized* per issue
    ///      #243, but a proxy: it refuses a boss whose last HP sync landed
    ///      above 10% before a burst finished it, and it cannot tell a
    ///      corpse from a near-dead enemy that vanished for some other
    ///      reason. `Dead` answers both directly. It is
    ///      corroborated as well as sourced: across the captured monster
    ///      despawns, `Dead` entries never came back except as six trash
    ///      uids reusing spawn slots on respawn (see
    ///      `pb::EDisappearType`'s evidence table).
    ///    - **`reason == Some(anything else)`** — refused outright, *even
    ///      when the health threshold would have been satisfied*. This is
    ///      the false-positive class issue #243 flagged: `Destroy` is the
    ///      mass-eviction reason, and an evicted boss that happened to be
    ///      burned low would otherwise end a live pull. The server's "not a
    ///      death" outranks our inference, in both directions.
    ///    - **`reason == None`** — no tag 2 on the packet, so fall back to
    ///      issue #215's heuristic unchanged: last observed health at or
    ///      below [`DESPAWN_DEATH_MAX_HP_PCT`], and observed *at all*. The
    ///      never-observed case stays refused on purpose: with neither a
    ///      reason nor health there is no evidence, and
    ///      `EnemyState::is_alive`'s conservative "unknown means alive"
    ///      must not be flipped by a packet carrying a uuid and nothing
    ///      else.
    ///
    /// Getting this wrong in the permissive direction is worse than the
    /// status quo: a range-out mid-pull would freeze the meter, save a
    /// truncated encounter, and split the rest of the fight into a second
    /// one. Getting it wrong in the strict direction costs only the instant
    /// freeze — the idle timeout still ends the fight, which is precisely
    /// today's behaviour. So every uncertain case is refused.
    ///
    /// When the rule does fire the despawn is routed through exactly the same
    /// machinery a death packet is: `mark_enemy_dead`, then `recompute_boss`,
    /// then `end_fight_on_boss_death` — which keeps its own guards
    /// (`other_living_boss`, `dungeon_objective_still_running`), so a
    /// multi-part boss or an instance whose own objective tracking says the
    /// run is still going still holds the pull open. The end is stamped at
    /// `last_event_ms`, the last real player damage, not at the despawn
    /// packet: the fight ended when the hitting stopped (the same rule the
    /// `Scene`/`SceneChanged` arm uses), and the despawn may trail it by
    /// seconds.
    ///
    /// The enemy is deliberately **not** removed from `enemies`. A despawn is
    /// not an invalidation — the entity keeps its uid and can stream straight
    /// back in — and the row is what `boss_monster_id` reads to caption the
    /// held fight. Only a `ProtocolEvent::ServerChanged` clears that map.
    ///
    /// **Verification status:** the decode side is exercised by unit tests
    /// against synthesized `SyncNearEntities` payloads. Tag 2 itself is
    /// verified — two independent reference sources plus 851 disappear
    /// entries across our own captures, tabulated on `pb::EDisappearType` —
    /// but no live `SHINRA_INSPECT=1` capture of a real *raid-boss* despawn
    /// has been taken yet (issue #215 asks for one, on the evidence bar
    /// issues #35 and #111 set). Until it has, this rule's field behaviour on
    /// scene 13023 is argued, not observed — which is the other reason it is
    /// written this tightly.
    fn apply_enemy_gone(&mut self, entity: EntityId, uid: i64, reason: Option<DisappearReason>) {
        if !self.fight_cfg.end_on_boss_death
            || self.fight_start_ms.is_none()
            || self.fight_end_ms.is_some()
            || self.boss_entity != Some(entity)
        {
            return;
        }
        let now_ms = self.last_event_ms;
        let Some(enemy) = self.enemies.get(&entity) else {
            return;
        };
        if !is_engaged_recognized_boss(enemy, now_ms) {
            return;
        }
        // Clause 8, issue #276: the server's own reason wins in both
        // directions, and only its absence falls back to issue #215's
        // health heuristic. `Some(_)` other than `Dead` is a deliberate
        // refusal, not a gap — see this function's doc comment.
        let plausibly_dead = match reason {
            Some(DisappearReason::Dead) => true,
            Some(_) => false,
            None => enemy
                .pct()
                .is_some_and(|pct| pct <= DESPAWN_DEATH_MAX_HP_PCT),
        };
        if !plausibly_dead {
            return;
        }
        log::info!(
            "encounter: treating despawn of uid={uid} monster_id={} reason={reason:?} as a death (issues #215/#276)",
            enemy.monster_id.map_or(-1i64, i64::from)
        );
        self.mark_enemy_dead(entity);
        self.recompute_boss();
        self.end_fight_on_boss_death(entity, now_ms);
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
    /// Catches the enemy `recompute_boss` cannot rank: one with neither
    /// `max_hp` nor `curr_hp` is filtered out of the ranking entirely, so a
    /// living damaged boss known only by its `monster_id` would otherwise be
    /// invisible and the dead phase's latch would fire over the top of it.
    /// (`recompute_boss` itself ranks a living recognized boss above a dead
    /// one, but both call sites now capture whether `dying_uid` was the
    /// tracked boss *before* calling it — issue #210/#211 — so this is no
    /// longer only a backstop for the unrankable case: it is consulted for
    /// every recognized-boss death.)
    ///
    /// Scene-aware since issue #210/#211, via *co-engagement* rather than a
    /// blanket scene check: a `phase::is_boss_select_scene` raid can hold
    /// several final bosses, but not all of them are "still being fought"
    /// in the same sense. Field of Forgotten Illusions' three selections
    /// are sequential — the next one sits untouched (or was poked once and
    /// abandoned) while the current one is being fought, so it must not
    /// hold this one's death open. Dreambloom Ruins' Caprahorn selection is
    /// the opposite: it spawns *two* equal-HP bosses fought concurrently in
    /// that same kind of scene (see `phase::BOSS_SELECT_SCENES`'s own
    /// comment on it), so its twin — still being actively hit — must keep
    /// holding the pull open exactly as it does outside a boss-select scene.
    ///
    /// The two read identically as "another living, damaged, recognized
    /// boss"; what tells them apart is recency, the same signal
    /// `engaged_boss_still_up` uses for the same reason (issue #210/#211):
    /// inside a boss-select scene, an other-boss candidate only counts if it
    /// was damaged within `BOSS_ENGAGEMENT_WINDOW_MS` of `now_ms`. Outside a
    /// boss-select scene the check stays unconditional, as it always has —
    /// an ordinary dungeon's multi-phase/multi-part pull (the Dragonbane
    /// Golem cannons, say) has no "next selection" to distinguish from.
    ///
    /// Returns the offending uid rather than a bare `bool` (issue #256) so
    /// `end_fight_on_boss_death`'s refusal diagnostic can name *which* enemy
    /// it thinks is still up — the single fact that separates a genuine
    /// concurrent pair from a stale row, and the one the log could not
    /// previously supply. Which uid is reported when several qualify is
    /// unspecified (`enemies` is a `HashMap`) — the guard's answer is the
    /// yes/no, and the uid is a diagnostic hint, not a contract.
    fn other_living_boss(&self, dying: EntityId, now_ms: u64) -> Option<EntityId> {
        let boss_select = self.scene_id.is_some_and(phase::is_boss_select_scene);
        self.enemies
            .iter()
            .find(|(id, e)| {
                **id != dying
                    && is_damaged_living_boss(e)
                    && (!boss_select || engaged_within_window(e.last_damaged_ms, Some(now_ms)))
            })
            .map(|(id, _)| *id)
    }

    /// Whether every party member the meter knows about is down *right
    /// now* (issue #154), i.e. the fight in progress is over and lost.
    ///
    /// The roster is `players`: every uid the meter has seen act, plus the
    /// party members preloaded from the game's own roster packet in an
    /// instance (issue #12/#145/#149). This reads `alive`, not `deaths >
    /// 0` (issue #212): `deaths` is a cumulative per-encounter counter that
    /// never comes back down, so in a long pull with battle rezzes it
    /// eventually goes nonzero on every row without the party ever being
    /// down at the same time — the moment the last still-standing player
    /// took their first death, every row read "has died", and the fight
    /// falsely latched a wipe mid-pull with everyone still fighting.
    /// `alive` tracks the current state instead: cleared by
    /// `record_death`, set again by `apply_damage` on the next event that
    /// player acts in — a heal they cast counts, since no
    /// player-HP-above-zero signal exists to prefer over it (see there) —
    /// and ordered by the event clock, so a stale packet cannot flip a
    /// corpse back up (`PlayerStats::set_alive`). An empty roster is never
    /// a wipe, and neither is a death outside a running fight.
    ///
    /// Detecting the wipe directly is what retires the HP-rollback
    /// heuristic for this case: the rollback shape depends on how fast a
    /// particular boss's bar refills relative to the 9s idle timeout, which
    /// is why the same wipe used to go either way.
    fn party_is_wiped(&self) -> bool {
        self.fight_start_ms.is_some()
            && self.fight_end_ms.is_none()
            && !self.players.is_empty()
            && self.players.values().all(|p| !p.alive)
    }

    /// The looser sibling of [`Self::party_is_wiped`], for the one caller
    /// that already holds independent proof the pull is over: at least
    /// [`WIPE_PARTY_DOWN_FRACTION`] of the roster is down *right now*
    /// (issue #259).
    ///
    /// Same `alive`-not-`deaths` reading as `party_is_wiped`, and for the
    /// same issue #212 reason — a cumulative death counter would make this
    /// creep true through any long pull with battle rezzes. The only
    /// difference is unanimity, which the rollback path can afford to drop
    /// (see [`WIPE_PARTY_DOWN_FRACTION`]) and the death path cannot.
    ///
    /// `party_is_wiped` implies this: everyone down is at least four in
    /// five down, for any non-empty roster.
    fn party_mostly_down(&self) -> bool {
        if self.fight_start_ms.is_none() || self.fight_end_ms.is_some() || self.players.is_empty() {
            return false;
        }
        let down = self.players.values().filter(|p| !p.alive).count();
        // Multiply rather than divide: no division by zero to reason about
        // (the empty roster is already refused above) and no rounding rule
        // to pick — a 15-player raid needs 12 down, a 4-player dungeon 4
        // (3.2 rounded up by the `>=`), which is the strict reading for a
        // roster too small to have room for a straggler.
        down as f64 >= self.players.len() as f64 * WIPE_PARTY_DOWN_FRACTION
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
    ///
    /// ...for [`WIPE_HOLD_RELEASE_MS`] after the wipe, and no longer (issue
    /// #204). "The next hit decides" assumes a next hit that *can* decide,
    /// and nothing on the wire guarantees one — leaving the hold, and with
    /// it every event `apply_damage` drops while a fight is held, wedged
    /// until the player zones or resets by hand. Past that bound the attempt
    /// is no longer being reviewed, so the recognized-boss test stops
    /// deciding and the ordinary issue #78 rule takes it: any real player
    /// damage is the next fight.
    fn withholds_after_wipe(&self, d: &DamageEvent) -> bool {
        self.wipe_hold
            && !self.wipe_hold_released(d.timestamp_ms)
            && !self
                .target_monster_id(d)
                .is_some_and(tables::is_boss_monster)
    }

    /// Whether the wipe hold has been held past [`WIPE_HOLD_RELEASE_MS`] as
    /// of `now_ms` and so no longer withholds anything (issue #204).
    ///
    /// `fight_end_ms` is the wipe: `withholds_after_wipe` is only ever
    /// consulted from the `NewFight` gate, which already requires a held
    /// fight, and `wipe_hold` is only ever set alongside that latch. A
    /// missing latch therefore cannot happen, and reading it as "not yet
    /// released" if it somehow did is the conservative answer.
    fn wipe_hold_released(&self, now_ms: u64) -> bool {
        self.fight_end_ms
            .is_some_and(|end_ms| now_ms.saturating_sub(end_ms) >= WIPE_HOLD_RELEASE_MS)
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
    ///
    /// Issue #316: fails open (never withholds) while `self.enemies` is
    /// completely empty — a `ServerChanged`/dungeon-entry reconnect clears
    /// it, and `ServerChanged`/dungeon-entry also now clear
    /// `fight_end_boss_id` alongside it (see those `apply` arms), so an
    /// empty map ordinarily means `armed_phase_hold` has already returned
    /// `None` and this whole function is moot. This is the belt on top of
    /// that suspenders: an empty map can never grow `d`'s target a
    /// `monster_id` on its own, so "undecided, wait for the `EnemyHp`" —
    /// correct for the *packet-order* case the third bullet above is about,
    /// where `enemies` still holds this fight's other entities — would
    /// otherwise wedge every hit shut until the window lapsed by itself.
    fn withholds_new_fight(&self, d: &DamageEvent) -> bool {
        if self.enemies.is_empty() {
            return false;
        }
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
        // issue #316: anchored on when the end was *observed* (latched),
        // not `fight_end_ms` (when it happened) — see
        // `fight_end_observed_ms`'s doc comment for why those differ for an
        // idle-timeout end and nowhere else.
        let (Some(observed_ms), Some(ended_by)) =
            (self.fight_end_observed_ms, self.fight_end_boss_id)
        else {
            return None;
        };
        if !phase::has_phase_group(ended_by) {
            return None;
        }
        if d.attacker_kind != EntityKind::Player || d.is_heal {
            return None;
        }
        if d.timestamp_ms.saturating_sub(observed_ms) > window {
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
        self.enemies.get(&target_key(d)).and_then(|e| e.monster_id)
    }

    /// Counts one death for `target_uid`, debounced by `DEATH_DEBOUNCE_MS`
    /// against the last death counted for the same uid (issue #49). Lazily
    /// creates the target's `PlayerStats` entry — a player can die without
    /// ever having attacked (e.g. a healer or a fresh join), so this cannot
    /// rely on an entry the attacker-side path in `apply_damage` already
    /// made.
    /// Counts one skill activation against its caster (issue #245).
    ///
    /// Deliberately inert beyond that count. A cast does not start the
    /// fight clock, does not advance `last_event_ms`, does not end a hold,
    /// and is not evidence of a revive — the attr it is decoded from rides
    /// every delta a player's client sends, in town as much as in a
    /// dungeon, so treating it as combat activity would keep the meter
    /// permanently "in a fight" and dilute every DPS figure with the walk
    /// back to the vendor.
    ///
    /// `get_mut`, not `entry`: same rule the breakdowns below follow. A
    /// stranger casting past the player must not open a row in a fight
    /// they are no part of, and a cast carries no damage to justify one.
    /// A cast that arrives while a finished fight is held on screen is
    /// dropped, unless it lands inside `FightConfig::post_end_grace_ms` of
    /// the end — the same grace window `apply_damage_grace` applies to
    /// damage/heal events, for the same reason: a cast packet decoded from
    /// the tail of the kill is not evidence of anything but that stream of
    /// packets still being in flight.
    fn apply_cast(&mut self, c: &CastEvent) {
        if self.fight_end_ms.is_some() && !self.in_post_end_grace_window(c.timestamp_ms) {
            return;
        }
        if let Some(stats) = self
            .players
            .get_mut(&c.caster.or_display(c.caster_uid, EntityKind::Player))
        {
            *stats.casts.entry(c.skill_id).or_insert(0) += 1;
        }
    }

    /// Opens (or refreshes) one in-flight buff interval (issue #267). Only
    /// enriches an existing row, like `record_breakdowns` — a buff landing
    /// on a uid with no row yet is not a participant in this encounter.
    ///
    /// A `buff_uuid` already active is a refresh/stack, not a fresh
    /// application: the original `start_ms` is kept (so re-applying a
    /// still-up buff does not reset its uptime clock), and `base_id` is
    /// backfilled only if this event supplies one the opening event didn't
    /// (see `ActiveBuff::base_id`'s doc comment). An `adds_layer` event
    /// (`StackLayer`) additionally grows the instance's layer count, so a
    /// later `RemoveLayer` sheds that layer rather than closing the whole
    /// interval — see `ActiveBuff::layers`.
    fn apply_buff_apply(
        &mut self,
        host: EntityId,
        buff_uuid: i32,
        base_id: Option<i32>,
        adds_layer: bool,
        timestamp_ms: u64,
    ) {
        // Grace window (`FightConfig::post_end_grace_ms`): a buff applied
        // in the last moments before a kill can decode after `fight_end_ms`
        // latches, same as a trailing damage packet — see
        // `apply_damage_grace`'s doc comment.
        if self.fight_end_ms.is_some() && !self.in_post_end_grace_window(timestamp_ms) {
            return;
        }
        let Some(stats) = self.players.get_mut(&host) else {
            return;
        };
        match stats.active_buffs.get_mut(&buff_uuid) {
            Some(active) => {
                if active.base_id.is_none() {
                    active.base_id = base_id;
                }
                if adds_layer {
                    active.layers += 1;
                }
            }
            None => {
                stats.active_buffs.insert(
                    buff_uuid,
                    ActiveBuff {
                        base_id,
                        start_ms: timestamp_ms,
                        layers: 1,
                    },
                );
            }
        }
    }

    /// Closes one in-flight buff interval (issue #267), crediting its
    /// duration to `PlayerStats::buffs[base_id]`. A `buff_uuid` with no
    /// open interval (no matching apply seen, or already closed — a
    /// retransmitted remove) is a no-op, and one whose interval never
    /// learned a `base_id` is dropped silently: there is nothing to
    /// attribute the uptime to (see `ProtocolEvent::BuffRemove`'s doc
    /// comment for why this happens for roughly half of real removes).
    ///
    /// A `removes_layer` event (`RemoveLayer`) only sheds one layer: the
    /// interval closes when the last one goes, which for the overwhelmingly
    /// common single-layer instance is that very event. A full `Remove`
    /// closes the interval outright, however many layers it held.
    fn apply_buff_remove(
        &mut self,
        host: EntityId,
        buff_uuid: i32,
        removes_layer: bool,
        timestamp_ms: u64,
    ) {
        // Grace window: a buff's closing `Remove`/`RemoveLayer` is exactly
        // the "buff closes" case `FightConfig::post_end_grace_ms` exists
        // for — without this, a buff still open when the fight ended would
        // never get the chance to credit its last stretch of uptime. See
        // `apply_damage_grace`'s doc comment.
        if self.fight_end_ms.is_some() && !self.in_post_end_grace_window(timestamp_ms) {
            return;
        }
        let Some(stats) = self.players.get_mut(&host) else {
            return;
        };
        if removes_layer && let Some(active) = stats.active_buffs.get_mut(&buff_uuid) {
            active.layers = active.layers.saturating_sub(1);
            if active.layers > 0 {
                return;
            }
        }
        let Some(active) = stats.active_buffs.remove(&buff_uuid) else {
            return;
        };
        let Some(base_id) = active.base_id else {
            return;
        };
        let entry = stats.buffs.entry(base_id).or_default();
        entry.total_uptime_ms += timestamp_ms.saturating_sub(active.start_ms);
        entry.apply_count += 1;
    }

    /// Accumulates one event into issue #245's per-tab breakdowns: the
    /// attacker's outgoing healing, and the target's incoming everything
    /// (damage taken and healing received alike).
    ///
    /// `get_mut`, never the `entry` API the outgoing-damage path uses —
    /// the same rule the revive detection in `apply_damage` follows. These
    /// views may enrich a row the roster already holds, but must never
    /// *open* one: a stranger healing their way past the player in town,
    /// or a field mob swinging at a passer-by, is not a participant in
    /// this encounter and must not appear as a row in it.
    fn record_breakdowns(&mut self, d: &DamageEvent) {
        if d.is_heal
            && d.attacker_kind == EntityKind::Player
            && let Some(stats) = self.players.get_mut(&attacker_key(d))
        {
            accumulate_skill(stats.heals.entry(d.skill_id).or_default(), d);
            if !d.is_miss {
                stats.total_heal += d.value;
            }
        }
        if d.target_kind == EntityKind::Player
            && let Some(stats) = self.players.get_mut(&target_key(d))
        {
            accumulate_skill(stats.incoming.entry(d.skill_id).or_default(), d);
            if !d.is_miss {
                stats.total_incoming += d.value;
            }
        }
    }

    fn record_death(&mut self, target: EntityId, target_uid: i64, timestamp_ms: u64) {
        let stats = self
            .players
            .entry(target)
            .or_insert_with(|| PlayerStats::new(target_uid));
        // issue #212: `deaths` is a cumulative counter — it never resets
        // once nonzero — so `party_is_wiped` cannot read it directly
        // without treating a battle-rezzed player as still down for the
        // rest of the pull. `alive` is the "right now" bit that fixes
        // that; set back to `true` in `apply_damage` on the next event
        // this player acts in.
        //
        // Above the debounce return, not below it: the debounce exists so
        // a retransmitted packet cannot count one death twice, and a
        // duplicate still reports a player who is down. The bit is
        // idempotent, so there is nothing there to protect it from.
        stats.set_alive(false, timestamp_ms);
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
        let key = p.entity.or_display(p.uid, EntityKind::Player);
        let merged = self.name_upsert(
            p.uid,
            CachedAttrs {
                name: p.name.clone(),
                class: p.class,
                ability_score: p.ability_score,
                season_strength: p.season_strength,
                imagines: p.imagines,
                imagine_tiers: p.imagine_tiers,
            },
        );
        if let Some(stats) = self.players.get_mut(&key) {
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
            self.players.insert(key, stats);
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
        let key = e.entity.or_display(e.uid, EntityKind::Monster);
        // `last_event_ms` is the DPS-window end and must reflect damage
        // only; enemy-HP sync/regen packets arriving after combat stops
        // would otherwise keep extending the denominator and decay DPS
        // toward zero with no combat happening.
        {
            let enemy = self.enemies.entry(key).or_default();
            if let Some(new_id) = e.monster_id {
                // issue #313/#317: this in-place rewrite is how a `uid=1`
                // boss silently went from monster_id 20004 ("Ignisor", a
                // `BOSS_MONSTER_IDS` entry) to 3000063 ("Denvel", which was
                // not one) mid-pull, with no `boss target changed` line
                // anywhere in the log — that diagnostic keys off the uid,
                // and the uid never moved. The #314 diagnostic this logs
                // settled the question in #317: `uid = uuid >> 16` makes
                // uid=1 the very first slot the game ever allocates, and
                // every curated group in `phase.rs` stays inside one id
                // family while 20004 -> 3000063 crosses id spaces — this is
                // a recycled uid naming a *different* entity, not one live
                // entity being re-templated. So on a real change (not the
                // first id ever seen for this uid, and not a resync
                // repeating the same id) the rest of `EnemyState` is reset
                // to a fresh entity here: `peak_hp`/`max_hp`/`curr_hp`/
                // `lowest_pct` described the old entity's HP pool, and
                // `took_damage`/`death_order`/`last_damaged_ms` described
                // its fight history, and none of that describes whatever
                // just took over this uid. The incoming packet's own HP
                // fields, applied below, become that fresh entity's first
                // observation.
                //
                // A delta that carries `monster_id` without `curr_hp`/
                // `max_hp` (an AOI-sync delta can — see
                // `enemy_hp_from_attrs`) resets to `EnemyState::default()`
                // same as any other change, so `curr_hp`/`max_hp` land as
                // `None` and `pct()` reads `None` until the next HP-bearing
                // packet for this uid arrives. That is deliberate, not an
                // oversight: the old entity's HP pool has nothing to say
                // about the new one's, so carrying it over would be a false
                // reading, not a stale-but-close one. The HP bar is expected
                // to blank out for the gap between the two packets.
                if let Some(msg) = monster_id_change_log(e.uid, enemy.monster_id, new_id) {
                    log::info!("{msg} — state reset");
                    *enemy = EnemyState {
                        monster_id: Some(new_id),
                        ..Default::default()
                    };
                } else {
                    enemy.monster_id = Some(new_id);
                }
            }
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
            self.mark_enemy_dead(key);
        }

        // issue #210/#211: captured *before* `recompute_boss`, which ranks a
        // living recognized boss above a dead one — so the instant this
        // sync's `curr_hp == 0` stamps `e.uid`'s death order above, any
        // other recognized boss already damaged and still alive outranks
        // the corpse and `recompute_boss` moves `boss_entity` off it. Mirrors
        // `apply_damage`'s capture: it answers whether `e.uid` was the
        // tracked boss when this update landed, which is the only form of
        // the question a *death* can still be asked after the ranking has
        // reacted to it.
        let was_tracked_boss = self.boss_entity == Some(key);

        self.recompute_boss();

        if was_tracked_boss {
            // issue #78: the boss's HP reaching 0 is the other end-of-fight
            // signal (the death packet can be missed; an HP sync to 0 is
            // hard to miss). Same recognized-boss gate as the death path.
            if self.fight_cfg.end_on_boss_death
                && self.enemies.get(&key).and_then(|x| x.curr_hp) == Some(0)
            {
                self.end_fight_on_boss_death(key, e.timestamp_ms);
            }
        }

        // PR #223 review, finding 1: deliberately the *post*-`recompute_boss`
        // identity, not `was_tracked_boss`. The two checks ask opposite
        // questions of the same sync. A death asks "was this the boss we were
        // following?", and only the pre-capture can answer it, because dying
        // is itself what demotes the enemy out of `boss_entity`. A rollback asks
        // "is the boss we are following the one whose bar just refilled?" —
        // and refilling is what *promotes* an enemy into `boss_entity`, since in
        // the tier-0 (`curr_hp`-only, issue #76) ranking the bar's height is
        // the rank. The canonical wipe is exactly that: the boss the party
        // burned to 20% snaps back to full in one sync, overtaking whatever
        // trash or sibling boss led the ranking while it was low. Gating that
        // on the pre-capture drops the reset on the floor, which is how the
        // wipe's damage keeps piling into the next pull.
        if self.boss_entity == Some(key) {
            let cooldown_ok = match self.last_reset_ms {
                Some(last) => e.timestamp_ms.saturating_sub(last) >= self.reset_cfg.cooldown_ms,
                None => true,
            };
            // issue #157: `boss_entity` is not only a display choice — it
            // selects whose HP bar the auto-reset heuristic watches, and
            // `recompute_boss` can park it on a trash mob (its candidate
            // set is "damaged in this fight", which after a reset the first
            // add hit by party AoE wins outright). Measuring a wipe off an
            // add is never right: adds die and respawn at full HP as a
            // matter of course, which is the rollback signature. Trash can
            // therefore never reset the meter, whatever the header is
            // pointing at.
            let should_reset = {
                let enemy = &self.enemies[&key];
                enemy.monster_id.is_some_and(tables::is_boss_monster)
                    && check_hp_rollback(enemy, &self.reset_cfg)
            };
            if should_reset {
                // issue #259: a rollback with the party on the floor is a
                // *wipe*, and a wipe is a fight end — the attempt is worth
                // keeping and worth recording. Ordered ahead of the reset
                // rather than left to race it: previously whichever path
                // happened to fire first decided whether the pull reached
                // the history database at all, so the same boss in the same
                // scene ended `cause=wipe` on one raid night and vanished as
                // `reset reason=BossHpRollback` on the next. The two are not
                // alternatives — the wipe describes the attempt that just
                // ended, the reset clears the slate for the next one — so
                // this latches the end and lets the `held` test below do the
                // deferring, which is the same mechanism the `Scene` arm
                // already relies on: a fight frozen in this very call has not
                // had one tick to be observed as `Ended` and recorded, and
                // resetting it here would erase it before anything outside
                // this crate ever saw it. The next pull's first hit on a
                // recognized boss clears the hold through `NewFight`.
                //
                // `party_mostly_down`, not `party_is_wiped`: the rollback is
                // itself the proof the pull is over, so the roster does not
                // have to be unanimous (see `WIPE_PARTY_DOWN_FRACTION`).
                // No `engaged_boss_still_up` gate either, the way the
                // death-packet path needs one — `should_reset` has already
                // established that the enemy this is measured off is a
                // recognized boss whose bar the party burned down and the
                // server put back, which is a stronger statement of the same
                // fact.
                if self.party_mostly_down() {
                    self.latch_fight_end(
                        FightEndCause::Wipe,
                        e.timestamp_ms,
                        e.timestamp_ms,
                        self.boss_monster_id(),
                    );
                    self.wipe_hold = true;
                }
                // issue #78: while the last fight's stats are held, a boss HP
                // bar refilling (the corpse resyncing, or the next party
                // pulling it) must not clear them. The hold is only ever
                // ended by combat the *user* is part of, or by an explicit
                // reset. Read *after* the wipe latch above, so a wipe this
                // same sync just latched is one of the holds it honours.
                let held = self.fight_ended_at(e.timestamp_ms).is_some();
                if cooldown_ok && !held {
                    self.reset(ResetReason::BossHpRollback, e.timestamp_ms);
                    return Some(ResetReason::BossHpRollback);
                }
                // The rollback shape was observed but suppressed (by the
                // cooldown gate, or by the post-fight hold). Latch it so the
                // same rollback can't re-fire the instant the cooldown
                // expires (it's level-triggered on `lowest_pct`, which only
                // clears inside `reset`).
                if let Some(enemy) = self.enemies.get_mut(&key) {
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
    /// at all, and demanding it left `boss_entity` — and so the header — empty
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
    ///    `boss_entity == target_uid` path instead of falling through to the
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
    ///    breaking ties on `hp` alone let `boss_entity` flip between calls for
    ///    two enemies sharing the same `max_hp`.
    ///
    /// An enemy with no HP of either kind is unrankable and stays out.
    fn recompute_boss(&mut self) {
        let previous_boss_entity = self.boss_entity;
        let damaged = self.rank_boss(|e| e.took_damage);
        // issue #157: an enemy the boss table does not recognize must not
        // hold the target while a recognized boss is still up in this
        // encounter — one boss or several, since a pull can have more than
        // one up at once. The candidate set above is "damaged in the current
        // fight", and `reset` clears `took_damage` on every enemy — so
        // immediately after any reset the set is empty and the first enemy
        // to take damage wins outright, no matter what it is. Party AoE
        // landing on adds first is the ordinary case, and the ranking key
        // cannot help, because the boss is not a candidate until someone
        // hits it. `is_alive`, `last_damaged_ms` and the recency window
        // together scope the fallback to a boss actually still being
        // fought: a corpse from the last pull cannot take the header back
        // off the trash the party has moved on to; neither can a boss
        // nobody has ever touched — one merely synced into the map by an
        // AOI `EnemyHp` packet would otherwise name the header and own the
        // HP bar while the party fights the adds (PR #163 review, finding
        // 2); and neither can a boss the party damaged, gave up on and
        // left standing, which "ever damaged and alive" alone matches
        // forever (PR #163 re-review). `last_damaged_ms` rather than
        // `took_damage`, because the whole point of the fallback is the
        // window where `reset` has just cleared the per-fight flag. Where
        // two bosses are up together the ordinary ranking picks between
        // them, exactly as it does once both have been damaged.
        //
        // The clock this recency is measured against is the damage clock of
        // the enemy currently holding the target, not a wall clock: the
        // question is whether the boss was being fought *around the time*
        // the thing now taking hits was, which is exactly what separates
        // the two cases (see `BOSS_ENGAGEMENT_WINDOW_MS`). It also keeps
        // `recompute_boss` callable from the `EnemyHp` path, which carries
        // no timestamp of its own.
        self.boss_entity = match damaged {
            Some(uid) if !self.is_recognized_boss(uid) => {
                let engaged_at = self.enemies.get(&uid).and_then(|e| e.last_damaged_ms);
                self.rank_boss(|e| {
                    e.is_alive()
                        && engaged_within_window(e.last_damaged_ms, engaged_at)
                        && e.monster_id.is_some_and(tables::is_boss_monster)
                })
                .or(Some(uid))
            }
            other => other,
        };

        let monster_id = self.boss_monster_id();

        // issue #152: remember which fight this is while it is still live,
        // so the header can keep naming it once the fight ends and zoning
        // out throws the live answer away. Only a recognized boss is worth
        // pinning — see `fight_identity` — and only a `Some` answer
        // overwrites, so an add that briefly wins `boss_entity` after the boss
        // dies cannot rename the fight that is being held.
        if let Some(id) = monster_id
            && tables::is_boss_monster(id)
        {
            self.fight_identity = Some(FightIdentity {
                boss_monster_id: id,
                scene_id: self.scene_id,
            });
        }

        // Sparse, transition-only diagnostic (issue #69): `recompute_boss`
        // runs on every damage/enemy-hp event, so this must only log when
        // the winner actually changes — logging every call would reproduce
        // the #87 flood at boss-target granularity instead of attr-id
        // granularity.
        if self.boss_entity != previous_boss_entity
            && let Some(msg) =
                boss_transition_log(previous_boss_entity, self.boss_entity, monster_id)
        {
            log::info!("{msg}");
        }
    }

    /// Ranks the enemies `candidate` accepts by the keys documented on
    /// [`Self::recompute_boss`] and returns the winner's uid, or `None`
    /// when nothing it accepted is rankable (an enemy with no HP of either
    /// kind stays out).
    ///
    /// Split out so `recompute_boss` can ask the same question of two
    /// candidate sets — the enemies damaged in this fight, and the
    /// recognized bosses still up (issue #157) — without duplicating the
    /// ranking.
    fn rank_boss(&self, candidate: impl Fn(&EnemyState) -> bool) -> Option<EntityId> {
        self.enemies
            .iter()
            .filter(|(_, e)| candidate(e))
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
            .map(|(uid, ..)| uid)
    }

    /// Whether `entity` names an enemy whose monster id is in the boss table.
    fn is_recognized_boss(&self, entity: EntityId) -> bool {
        self.enemies
            .get(&entity)
            .and_then(|e| e.monster_id)
            .is_some_and(tables::is_boss_monster)
    }

    /// Clears `players` and per-enemy `lowest_pct`; keeps `names`. Deaths
    /// are per-encounter (issue #49): `players.clear()` drops the whole
    /// `PlayerStats` entry per uid, taking `deaths`/`last_death_ms` with it,
    /// so no separate clearing step is needed here.
    ///
    pub fn reset(&mut self, reason: ResetReason, now_ms: u64) {
        // `reset` is itself already an event, never a per-snapshot poll, so
        // this is naturally sparse (issue #69) — no transition-only guard
        // needed the way scene/boss logging above requires one.
        let boss_hp_pct = self
            .boss_entity
            .and_then(|uid| self.enemies.get(&uid))
            .and_then(|e| e.pct());
        // issue #284: live down-state, not cumulative deaths — see
        // `party_is_wiped`'s doc comment for why `deaths > 0` is the wrong
        // read (a battle rez can never bring that counter back down, so it
        // stays true for the rest of the pull and makes a correct decision
        // read exactly like #259's original bug evidence).
        let party_down = self.players.values().filter(|p| !p.alive).count();
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
        // issue #152: the held fight's identity is released with the hold
        // itself, never before it and never after — every reset reason below
        // is also a reason the header should follow live state again.
        self.fight_identity = None;
        // Every reset reason (manual, boss-HP rollback, server change, and
        // the next fight's first hit) drops the post-fight hold: the numbers
        // being held belong to the encounter that is being cleared.
        self.fight_end_ms = None;
        self.fight_end_observed_ms = None;
        // ...and with it the phase-resume arming (issue #124): the fight
        // whose boss died is gone, so nothing can be a continuation of it.
        self.fight_end_boss_id = None;
        // ...and the wipe hold (issue #154): the attempt it was protecting
        // is what just got cleared.
        self.wipe_hold = false;
        self.last_reset_ms = Some(now_ms);
        // No enemy has `took_damage` anymore, so this clears `boss_entity`.
        // Otherwise a stale `boss_entity` from the previous pull would still
        // match an `EnemyHp` packet for the old boss arriving before the
        // next damage event, letting its HP-refill curve fire a second,
        // spurious reset.
        self.recompute_boss();
    }

    pub fn snapshot(&self, now_ms: u64) -> Snapshot {
        self.snapshot_focused(now_ms, None)
    }

    /// Same as [`Meter::snapshot`], but skips the four breakdown-tab
    /// vectors (`heals`, `dealt`, `received`, `casts`) for any player not
    /// named in `focus` — building only `skills`, which every row needs for
    /// the always-visible Dps bar.
    ///
    /// `focus` is the set of player entity ids (`EntityId.0`, issue #335 —
    /// not the display uid) with an open skill-breakdown
    /// window (`crates/app/src/ui.rs`'s `skill_windows` keys), threaded in
    /// from the UI via `UiCommand::SkillFocus`
    /// (`crates/app/src/pipeline.rs`'s live publish loop). `None` means
    /// "build every breakdown for every player" — [`Meter::snapshot`]'s own
    /// behavior, and every non-live caller (tests, replay/history, the
    /// sanitizer) goes through that path unchanged.
    ///
    /// This exists because the live pipeline (`crates/app/src/pipeline.rs`)
    /// publishes a snapshot ~10x/second regardless of whether any skill
    /// window is even open, and a skill window is closed almost all the
    /// time (PR #268 review, finding 2): sorting four extra `Vec<SkillRow>`
    /// per player on every tick for tabs nobody is looking at is pure
    /// waste. Gating by player rather than by (player, tab) keeps this
    /// crate ignorant of `bpsr-app`'s `SkillTab` type, and means flipping
    /// tabs on an already-open window never has to wait a tick for data
    /// that was already being built for that player.
    pub fn snapshot_focused(&self, now_ms: u64, focus: Option<&[i64]>) -> Snapshot {
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
            .iter()
            .map(|(entity, p)| {
                let dps = p.total_damage as f64 / (dps_duration_ms as f64 / 1000.0);
                let share_pct = if total_damage > 0 {
                    (p.total_damage as f64 / total_damage as f64 * 100.0) as f32
                } else {
                    0.0
                };
                let skills = skill_rows(p, dps_duration_ms);
                // issue #245, gated per PR #268 review finding 2: one
                // vector per breakdown tab, built here rather than lazily on
                // the UI side because every one of them needs
                // `dps_duration_ms`, which is snapshot-local. `focus` limits
                // this real work to players whose skill window is actually
                // open (see `snapshot_focused`'s doc comment) — everyone
                // else gets the same empty vectors `SkillTab::rows` already
                // returns for `Buff`, which is indistinguishable from "no
                // events yet" to every consumer since none is looking.
                let wants_breakdowns =
                    focus.is_none_or(|entities| entities.contains(&(entity.0 as i64)));
                let (heals, dealt, received, casts, buffs) = if wants_breakdowns {
                    (
                        breakdown_rows(&p.heals, p.total_heal, dps_duration_ms),
                        dealt_rows(p, dps_duration_ms),
                        breakdown_rows(&p.incoming, p.total_incoming, dps_duration_ms),
                        cast_rows(p, dps_duration_ms),
                        buff_rows(p, dps_duration_ms),
                    )
                } else {
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
                };
                PlayerRow {
                    uid: p.uid,
                    entity: entity.0 as i64,
                    name: p
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Player {}", p.uid)),
                    class: p.class,
                    ability_score: p.ability_score,
                    season_strength: p.season_strength,
                    imagines: p.imagines.unwrap_or_default(),
                    imagine_tiers: p.imagine_tiers.unwrap_or_default(),
                    damage: p.total_damage,
                    dps,
                    share_pct,
                    crit_pct: p.crit_pct(),
                    lucky_pct: p.lucky_pct(),
                    hits: p.hits,
                    deaths: p.deaths,
                    // Issue #254: `effective_now_ms`, not `now_ms` — an
                    // ended fight's rows are frozen as of its end, and a
                    // death still open then stops accruing with them.
                    dead_ms: Some(p.dead_ms_as_of(effective_now_ms)),
                    skills,
                    heals,
                    dealt,
                    received,
                    casts,
                    buffs,
                }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.damage));

        let total_dps = total_damage as f64 / (dps_duration_ms as f64 / 1000.0);

        // issue #152: while a finished fight is held on screen, the header
        // names *that* fight rather than whatever is live. The rows, totals
        // and clock below it are already frozen as of the fight's end, and
        // zoning out (`ServerChanged`, then the town's `Scene`) wipes the
        // live boss and scene while they stay frozen — so following live
        // state here captions a raid's damage breakdown with the town the
        // player just walked into. The hold releases via `reset`, which
        // clears `fight_identity` alongside everything else.
        let held = match self.fight_state(now_ms) {
            FightState::Ended => self.fight_identity,
            FightState::Active | FightState::Idle => None,
        };
        let (boss_monster_id, scene_id) = match held {
            // A scene the held fight never captured falls back to whatever
            // the meter knows now. The case that matters is an `EnterScene`
            // landing after the pull's last damage/HP event (`replay_dump`'s
            // real capture does exactly that), where "now" is that same
            // scene; a fight that never knew its scene at all has no better
            // answer to offer than the current one.
            Some(held) => (Some(held.boss_monster_id), held.scene_id.or(self.scene_id)),
            None => (self.boss_monster_id(), self.scene_id),
        };
        // issue #42: `recompute_boss` prefers a recognized boss but still
        // falls back to an HP heuristic when no monster in the pull is in the
        // table, so `boss_monster_id` alone can't tell a real boss from a big
        // trash mob. Gate the *display* fields
        // on `tables::is_boss_monster`; `boss_monster_id` itself stays
        // populated for every pull since it's real data, not a display
        // choice.
        let is_boss = boss_monster_id.is_some_and(tables::is_boss_monster);
        // issue #125, rewritten by issue #201: the dungeon's final boss, if
        // `scene_id` is a single-boss dungeon someone has written down in
        // `tables::SCENE_FINAL_BOSSES`. This used to be *learned* at runtime
        // and cached to disk; there are few enough dungeons in the game that
        // a curated table is both simpler and never wrong about a scene it
        // covers. Independent of `boss_monster_id`/`is_boss` above, which
        // stay the raw facts about the currently-selected target: a
        // genuinely recognized live boss (`is_boss`) wins over this field in
        // `encounter_title` (`crates/app/src/ui.rs`), which is the caption
        // for "nothing engaged yet" and for a non-boss `boss_entity` target —
        // see that function's doc comment for the full precedence and why.
        //
        // issue #150: a scene that lets the party pick which boss to pull has
        // no single right answer to fall back on, so it names none and
        // `encounter_title` shows "Select a boss" instead. Curated entries
        // are single-boss dungeons only (`phase`'s
        // `no_curated_scene_final_boss_is_a_boss_select_scene` guards that),
        // so this filter is belt-and-braces rather than load-bearing — but it
        // keeps the two curated tables from ever contradicting each other
        // silently.
        let multi_boss_scene = scene_id.is_some_and(phase::is_boss_select_scene);
        let scene_boss_name = scene_id
            .filter(|_| !multi_boss_scene)
            .and_then(tables::scene_final_boss)
            .and_then(tables::monster_name);
        let encounter = EncounterInfo {
            boss_monster_id,
            boss_name: if is_boss {
                boss_monster_id.and_then(tables::monster_name)
            } else {
                None
            },
            is_boss,
            scene_id,
            scene_name: scene_id.and_then(tables::scene_name),
            scene_boss_name,
            multi_boss_scene,
        };

        Snapshot {
            duration_ms: display_duration_ms,
            total_damage,
            total_dps,
            rows,
            encounter,
            // The meter has no notion of capture; `bpsr_app::pipeline` is
            // the only place that ever flips this (see `Snapshot::capture_alive`).
            capture_alive: true,
        }
    }

    /// Whether a fight's stats are on the board — true both while it is
    /// running and while an ended fight is being held (issue #78). Use
    /// [`Meter::fight_state`] to tell those two apart.
    pub fn is_active(&self) -> bool {
        self.fight_start_ms.is_some()
    }
}

/// Builds one `SkillRow` from a skill's raw accumulator and the player's
/// total damage (issue #16) — the single source of truth for the per-skill
/// arithmetic (share/crit/avg/hits-per-min), so both the real aggregator
/// ([`skill_rows`], below) and `bpsr-app`'s demo seed (`demo_skill_rows`) go
/// through the same formulas and can never quietly drift apart. Takes
/// `&SkillStats` rather than its fields spelled out (`lucky_hits`/
/// `lucky_damage` go unused here) so the arg count stays reasonable.
/// `dps_duration_ms` is `Meter::snapshot`'s shared DPS denominator (D8), so
/// `hits_per_min` can never disagree with the row's own DPS window.
pub fn skill_row_from_stats(
    skill_id: i32,
    skill: &SkillStats,
    player_damage: i64,
    dps_duration_ms: u64,
) -> SkillRow {
    let share_pct = if player_damage > 0 {
        (skill.total_damage as f64 / player_damage as f64 * 100.0) as f32
    } else {
        0.0
    };
    let crit_pct = if skill.hits > 0 {
        skill.crit_hits as f32 / skill.hits as f32 * 100.0
    } else {
        0.0
    };
    let avg = if skill.hits > 0 {
        skill.total_damage as f64 / skill.hits as f64
    } else {
        0.0
    };
    let avg_crit = if skill.crit_hits > 0 {
        skill.crit_damage as f64 / skill.crit_hits as f64
    } else {
        0.0
    };
    let white_hits = skill.hits - skill.crit_hits;
    let avg_white = if white_hits > 0 {
        (skill.total_damage - skill.crit_damage) as f64 / white_hits as f64
    } else {
        0.0
    };
    let hits_per_min = skill.hits as f64 / (dps_duration_ms as f64 / 60_000.0);
    SkillRow {
        skill_id,
        damage: skill.total_damage,
        share_pct,
        crit_pct,
        max_crit: skill.max_crit,
        avg_crit,
        avg_white,
        avg,
        hits: skill.hits,
        crit_hits: skill.crit_hits,
        hits_per_min,
    }
}

/// Builds one player's `SkillRow`s from their skill accumulators, damage
/// descending (issue #16) — split out from `Meter::snapshot` as a pure
/// function so the per-skill arithmetic sits beside its own unit tests
/// rather than buried in the row-building closure.
fn skill_rows(stats: &PlayerStats, dps_duration_ms: u64) -> Vec<SkillRow> {
    breakdown_rows(&stats.skills, stats.total_damage, dps_duration_ms)
}

/// Folds one event's amount/crit/lucky facts into a per-skill accumulator
/// (issue #245), mirroring `Meter::apply_damage`'s own outgoing-damage
/// bookkeeping exactly: `hits` counts the swing whether or not it landed
/// (a miss is a use of the skill, not a non-event), while every amount is
/// gated on `!is_miss`.
fn accumulate_skill(skill: &mut SkillStats, d: &DamageEvent) {
    skill.hits += 1;
    if d.is_miss {
        return;
    }
    skill.total_damage += d.value;
    if d.crit {
        skill.crit_hits += 1;
        skill.crit_damage += d.value;
        skill.max_crit = skill.max_crit.max(d.value);
    }
    if d.lucky {
        skill.lucky_hits += 1;
        skill.lucky_damage += d.value;
    }
}

/// Turns any per-skill accumulator map into display rows, amount-descending
/// (issue #245). The generalisation of the original `skill_rows`: `total`
/// is whichever player total that map's `% ` column divides by — damage
/// dealt, healing done, or amount received — so the four breakdown tabs
/// share one arithmetic path (`skill_row_from_stats`) and can never drift
/// apart in how a share, an average or a hit rate is computed.
fn breakdown_rows(
    skills: &HashMap<i32, SkillStats>,
    total: i64,
    dps_duration_ms: u64,
) -> Vec<SkillRow> {
    let mut rows: Vec<SkillRow> = skills
        .iter()
        .map(|(&skill_id, skill)| skill_row_from_stats(skill_id, skill, total, dps_duration_ms))
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.damage));
    rows
}

/// The Skill casts tab's rows (issue #245): one row per skill this player
/// has begun, cast-count-descending.
///
/// Goes through `skill_row_from_stats` like every other breakdown so the
/// per-minute rate shares the Dps tab's denominator exactly — a cast rate
/// and a hit rate that disagreed about how long the fight was would be
/// worse than either alone. Every amount-shaped field falls out as `0`
/// from an all-zero `SkillStats`, which is correct: a cast has no amount,
/// and the tab shows no amount column.
fn cast_rows(stats: &PlayerStats, dps_duration_ms: u64) -> Vec<SkillRow> {
    let mut rows: Vec<SkillRow> = stats
        .casts
        .iter()
        .map(|(&skill_id, &count)| {
            let stats = SkillStats {
                hits: count,
                ..Default::default()
            };
            skill_row_from_stats(skill_id, &stats, 0, dps_duration_ms)
        })
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.hits));
    rows
}

/// The "Skill dealt" tab's rows (issue #245): everything this player put
/// out, damage and healing merged under one skill id. A skill that both
/// damages and heals lands in both accumulators and is summed here, which
/// is what "amount dealt" means for it — `SkillStats::merge` maxes
/// `max_crit` rather than summing it, being a running max.
fn dealt_rows(stats: &PlayerStats, dps_duration_ms: u64) -> Vec<SkillRow> {
    let mut merged = stats.skills.clone();
    for (&skill_id, heal) in &stats.heals {
        merged.entry(skill_id).or_default().merge(heal);
    }
    breakdown_rows(
        &merged,
        stats.total_damage + stats.total_heal,
        dps_duration_ms,
    )
}

/// The Buff tab's rows (issue #267): this player's per-buff-type uptime,
/// uptime-descending. Built from `PlayerStats::buffs` only — a buff still
/// active when the snapshot is taken contributes nothing until its interval
/// closes (see `BuffStats`'s doc comment).
///
/// Reuses `SkillRow` verbatim, like every other breakdown tab — see
/// `PlayerRow::buffs`'s doc comment for the field remapping.
fn buff_rows(stats: &PlayerStats, dps_duration_ms: u64) -> Vec<SkillRow> {
    let mut rows: Vec<SkillRow> = stats
        .buffs
        .iter()
        .map(|(&base_id, buff)| SkillRow {
            skill_id: base_id,
            damage: buff.total_uptime_ms as i64,
            share_pct: if dps_duration_ms > 0 {
                (buff.total_uptime_ms as f64 / dps_duration_ms as f64 * 100.0) as f32
            } else {
                0.0
            },
            crit_pct: 0.0,
            max_crit: 0,
            avg_crit: 0.0,
            avg_white: 0.0,
            avg: if buff.apply_count > 0 {
                buff.total_uptime_ms as f64 / buff.apply_count as f64
            } else {
                0.0
            },
            hits: buff.apply_count as u64,
            crit_hits: 0,
            hits_per_min: 0.0,
        })
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.damage));
    rows
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
    previous: Option<EntityId>,
    new: Option<EntityId>,
    monster_id: Option<u32>,
) -> Option<String> {
    if previous == new {
        return None;
    }
    Some(match new.map(EntityId::display_uid) {
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

/// Builds the "monster id changed" diagnostic line (issue #313), or `None`
/// when nothing actually changed — `old` unknown (the first id ever seen for
/// this uid, which [`boss_transition_log`] already covers when it matters) or
/// `old == new`. Transition-only, in the issue #69 idiom: `apply_enemy_hp`
/// runs on every enemy-HP sync, so an unconditional line here would reproduce
/// the #87 flood.
///
/// Fills the blind spot issue #313 named: `boss_transition_log` keys off the
/// boss *uid*, so an entity whose `monster_id` is rewritten under a stable
/// uid — recognized boss to unrecognized, in the reported case — changes the
/// meter's whole read of the fight while emitting nothing at all. Carries
/// both catalogued names so the two ids are readable without a table lookup.
/// Never a player name or uid (`crates/app/src/logging.rs`); an enemy uid is
/// not player data. Pure, like the builders around it, for the same
/// testability reason.
fn monster_id_change_log(uid: i64, old: Option<u32>, new: u32) -> Option<String> {
    let old = old.filter(|id| *id != new)?;
    let old_name = tables::monster_name(old).unwrap_or("<unresolved>");
    let new_name = tables::monster_name(new).unwrap_or("<unresolved>");
    let recognized = tables::is_boss_monster(new);
    Some(format!(
        "encounter: monster id changed for uid={uid} from monster_id={old} name={old_name} to monster_id={new} name={new_name} recognized_boss={recognized}"
    ))
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

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    /// An enemy's key in `Meter::enemies` for display uid `u` (issue #335).
    /// These tests build events from display uids alone, so the key is the
    /// canonical reconstruction `EntityId::or_display` derives — the same
    /// one the meter files them under.
    fn ek(u: i64) -> EntityId {
        EntityId::from_display_uid(u, EntityKind::Monster)
    }

    /// A player's key in `Meter::players` for display uid `u`.
    fn pk(u: i64) -> EntityId {
        EntityId::from_display_uid(u, EntityKind::Player)
    }

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

    // -- issue #335: stable entity ids ------------------------------------

    /// Two entities that share a display uid but not a uuid — a recycled
    /// uid, or a shadow/mirror copy of a live entity — must keep separate
    /// stats. Before #335 the meter keyed on `uuid >> 16`, so both of these
    /// landed in one row and their damage blended.
    #[test]
    fn two_entities_sharing_a_display_uid_keep_separate_damage_totals() {
        let first = EntityId::from_display_uid(7, EntityKind::Player);
        // Same `uuid >> 16`, one extra flag bit: a distinct entity the old
        // truncation could not see.
        let second = EntityId::from_uuid(first.uuid() | (1 << 14));
        assert_eq!(first.display_uid(), second.display_uid());

        let hit = |attacker: EntityId, value: i64, ts: u64| {
            ProtocolEvent::Damage(DamageEvent {
                attacker,
                attacker_uid: attacker.display_uid(),
                attacker_kind: EntityKind::Player,
                value,
                timestamp_ms: ts,
                ..Default::default()
            })
        };

        let mut m = Meter::new();
        m.apply(&hit(first, 100, 1_000));
        m.apply(&hit(second, 250, 1_100));

        let snap = m.snapshot(1_100);
        assert_eq!(snap.rows.len(), 2, "one row per entity, not per uid");
        let mut totals: Vec<i64> = snap.rows.iter().map(|r| r.damage).collect();
        totals.sort_unstable();
        assert_eq!(totals, vec![100, 250]);
        // Both still *print* the same display uid — that is the number the
        // game shows, and #335 does not change it.
        assert!(snap.rows.iter().all(|r| r.uid == 7));
    }

    /// The same separation on the enemy side: a recycled monster uid must
    /// not inherit the previous holder's engagement or HP state.
    #[test]
    fn two_enemies_sharing_a_display_uid_keep_separate_state() {
        let first = EntityId::from_display_uid(9, EntityKind::Monster);
        let second = EntityId::from_uuid(first.uuid() | (1 << 15));

        let mut m = Meter::new();
        for (entity, curr, max) in [(first, 100u64, 100u64), (second, 500, 900)] {
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity,
                uid: entity.display_uid(),
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: None,
                timestamp_ms: 1_000,
            }));
        }
        assert_eq!(m.enemies.len(), 2);
        assert_eq!(m.enemies[&first].max_hp, Some(100));
        assert_eq!(m.enemies[&second].max_hp, Some(900));
    }

    // -- issue #245: Heal / Skill dealt / Skill received breakdowns -------

    fn heal(
        attacker_uid: i64,
        target_uid: i64,
        skill_id: i32,
        value: i64,
        ts: u64,
    ) -> ProtocolEvent {
        ProtocolEvent::Damage(DamageEvent {
            attacker_uid,
            attacker_kind: EntityKind::Player,
            target_uid,
            target_kind: EntityKind::Player,
            skill_id,
            value,
            is_heal: true,
            timestamp_ms: ts,
            ..Default::default()
        })
    }

    fn hit_on_player(
        attacker_uid: i64,
        attacker_kind: EntityKind,
        target_uid: i64,
        skill_id: i32,
        value: i64,
        ts: u64,
    ) -> ProtocolEvent {
        ProtocolEvent::Damage(DamageEvent {
            attacker_uid,
            attacker_kind,
            target_uid,
            target_kind: EntityKind::Player,
            skill_id,
            value,
            timestamp_ms: ts,
            ..Default::default()
        })
    }

    fn cast(caster_uid: i64, skill_id: i32, ts: u64) -> ProtocolEvent {
        ProtocolEvent::Cast(CastEvent {
            caster: EntityId::from_display_uid(caster_uid, EntityKind::Player),
            caster_uid,
            skill_id,
            timestamp_ms: ts,
        })
    }

    #[test]
    fn casts_are_counted_per_skill_for_a_player_in_the_roster() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 1000));
        m.apply(&cast(1, 1550, 1100));
        m.apply(&cast(1, 1550, 1200));
        m.apply(&cast(1, 1551, 1300));
        let snap = m.snapshot(2000);
        let row = &snap.rows[0];
        assert_eq!(row.casts.len(), 2);
        // Cast-count-descending, like every other breakdown's amount.
        assert_eq!(row.casts[0].skill_id, 1550);
        assert_eq!(row.casts[0].hits, 2);
        assert_eq!(row.casts[1].skill_id, 1551);
        assert_eq!(row.casts[1].hits, 1);
        // A cast has no amount, and the tab shows no amount column.
        assert_eq!(row.casts[0].damage, 0);
        assert_eq!(row.casts[0].share_pct, 0.0);
    }

    /// A cast rides every delta a player's client sends, in town as much
    /// as in a dungeon — so it must not open a row, start the fight clock,
    /// or advance the idle deadline.
    #[test]
    fn a_cast_alone_never_opens_a_row_or_starts_a_fight() {
        let mut m = Meter::new();
        m.apply(&cast(1, 1550, 1000));
        assert!(m.snapshot(2000).rows.is_empty());
        assert!(!m.is_active());
    }

    #[test]
    fn casts_share_the_dps_windows_per_minute_denominator() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&dmg(1, 100, 5_000));
        for ts in 0..6 {
            m.apply(&cast(1, 1550, ts * 1_000));
        }
        let snap = m.snapshot(5_000);
        let row = &snap.rows[0];
        // Two hits and six casts over the same window, so the cast rate is
        // exactly three times the Dps tab's hit rate for the same skill —
        // an assertion that pins the shared denominator without restating
        // how `snapshot` computes it.
        assert_eq!(row.skills[0].hits, 2);
        assert_eq!(row.casts[0].hits, 6);
        assert!(
            (row.casts[0].hits_per_min - 3.0 * row.skills[0].hits_per_min).abs() < 0.01,
            "cast rate {} vs hit rate {}",
            row.casts[0].hits_per_min,
            row.skills[0].hits_per_min
        );
    }

    // -- issue #267: Buff tab ---------------------------------------------

    fn buff_apply(host_uid: i64, buff_uuid: i32, base_id: Option<i32>, ts: u64) -> ProtocolEvent {
        ProtocolEvent::BuffApply {
            host: EntityId::from_display_uid(host_uid, EntityKind::Player),
            host_uid,
            buff_uuid,
            base_id,
            adds_layer: false,
            timestamp_ms: ts,
        }
    }

    /// The `StackLayer` shape of an apply: a new layer on an instance that
    /// is already up.
    fn buff_stack(host_uid: i64, buff_uuid: i32, ts: u64) -> ProtocolEvent {
        ProtocolEvent::BuffApply {
            host: EntityId::from_display_uid(host_uid, EntityKind::Player),
            host_uid,
            buff_uuid,
            base_id: None,
            adds_layer: true,
            timestamp_ms: ts,
        }
    }

    /// The full `Remove`: the whole instance, however many layers.
    fn buff_remove(host_uid: i64, buff_uuid: i32, ts: u64) -> ProtocolEvent {
        ProtocolEvent::BuffRemove {
            host: EntityId::from_display_uid(host_uid, EntityKind::Player),
            host_uid,
            buff_uuid,
            removes_layer: false,
            timestamp_ms: ts,
        }
    }

    /// The `RemoveLayer` shape: one layer off, which for a single-layer
    /// instance is the whole thing.
    fn buff_remove_layer(host_uid: i64, buff_uuid: i32, ts: u64) -> ProtocolEvent {
        ProtocolEvent::BuffRemove {
            host: EntityId::from_display_uid(host_uid, EntityKind::Player),
            host_uid,
            buff_uuid,
            removes_layer: true,
            timestamp_ms: ts,
        }
    }

    /// The dominant real-capture shape (see `pb::EBuffEventType`): one
    /// `AddTo` closed by one `RemoveLayer`. Layer accounting must not
    /// change it — the single layer it opened with is the one shed, so the
    /// interval closes exactly as a full `Remove` would.
    #[test]
    fn a_single_layer_buff_is_closed_by_one_remove_layer() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_remove_layer(1, 417, 4_000));
        let snap = m.snapshot(5_000);
        assert_eq!(snap.rows[0].buffs[0].damage, 3_000);
        assert_eq!(snap.rows[0].buffs[0].hits, 1);
    }

    /// Issue #267 follow-up: a stacking buff that sheds one of two layers
    /// stays up. Closing on the first `RemoveLayer` would have ended the
    /// interval early and left the rest of the buff's uptime uncounted (the
    /// reopened interval would carry no `base_id` and be dropped).
    #[test]
    fn shedding_one_of_two_layers_keeps_the_buff_up() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_stack(1, 417, 2_000));
        m.apply(&buff_remove_layer(1, 417, 3_000));
        // Still up: nothing closed, so nothing is credited yet.
        assert!(m.snapshot(3_500).rows[0].buffs.is_empty());
        m.apply(&buff_remove_layer(1, 417, 6_000));
        let snap = m.snapshot(7_000);
        // One continuous interval from the original start, not two.
        assert_eq!(snap.rows[0].buffs.len(), 1);
        assert_eq!(snap.rows[0].buffs[0].damage, 5_000);
        assert_eq!(snap.rows[0].buffs[0].hits, 1);
    }

    /// A full `Remove` ends a multi-layer instance outright, rather than
    /// leaving it up with its remaining layers.
    #[test]
    fn a_full_remove_closes_a_multi_layer_buff() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_stack(1, 417, 2_000));
        m.apply(&buff_stack(1, 417, 2_500));
        m.apply(&buff_remove(1, 417, 4_000));
        let snap = m.snapshot(5_000);
        assert_eq!(snap.rows[0].buffs[0].damage, 3_000);
        assert_eq!(snap.rows[0].buffs[0].hits, 1);
        // Truly closed: a stray remove afterwards finds nothing to credit.
        m.apply(&buff_remove(1, 417, 6_000));
        assert_eq!(m.snapshot(7_000).rows[0].buffs[0].hits, 1);
    }

    #[test]
    fn a_closed_apply_remove_interval_credits_uptime_to_its_base_id() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0)); // open the row
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_remove(1, 417, 4_000));
        let snap = m.snapshot(5_000);
        let row = &snap.rows[0];
        assert_eq!(row.buffs.len(), 1);
        assert_eq!(row.buffs[0].skill_id, 3_210_031);
        assert_eq!(row.buffs[0].damage, 3_000); // uptime_ms
        assert_eq!(row.buffs[0].hits, 1); // apply_count
        assert!((row.buffs[0].avg - 3_000.0).abs() < 0.01);
    }

    /// A buff still active when the snapshot is read contributes nothing
    /// yet — see `BuffStats`'s doc comment for why this is an accepted v1
    /// undercount rather than a wrong number.
    #[test]
    fn a_still_active_buff_contributes_no_uptime_until_it_closes() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        let snap = m.snapshot(5_000);
        assert!(snap.rows[0].buffs.is_empty());
    }

    /// A `BuffRemove` with no matching open interval (a stray/duplicate
    /// event) must not panic or fabricate a row.
    #[test]
    fn a_remove_with_no_matching_apply_is_a_no_op() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_remove(1, 417, 4_000));
        assert!(m.snapshot(5_000).rows[0].buffs.is_empty());
    }

    /// A remove event never carries a `base_id` (see
    /// `ProtocolEvent::BuffRemove`'s doc comment) — an interval that never
    /// learned one from its apply event is dropped rather than attributed
    /// to a fabricated id.
    #[test]
    fn an_interval_that_never_learned_a_base_id_is_dropped_on_remove() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, None, 1_000));
        m.apply(&buff_remove(1, 417, 4_000));
        assert!(m.snapshot(5_000).rows[0].buffs.is_empty());
    }

    /// A refresh/stack (a second `BuffApply` for the same `buff_uuid`
    /// before it closes) must not reset the uptime clock — the interval
    /// keeps its original `start_ms`.
    #[test]
    fn reapplying_a_still_active_buff_keeps_its_original_start() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 2_000)); // refresh
        m.apply(&buff_remove(1, 417, 4_000));
        let snap = m.snapshot(5_000);
        // 4_000 - 1_000, not 4_000 - 2_000: the refresh did not restart the
        // clock.
        assert_eq!(snap.rows[0].buffs[0].damage, 3_000);
        assert_eq!(snap.rows[0].buffs[0].hits, 1);
    }

    /// A later apply-like event for the same instance can backfill a
    /// `base_id` the opening one didn't carry (issue #267's common case —
    /// roughly half of real `AddTo` events carry no double-encoded
    /// `BuffInfo`).
    #[test]
    fn a_later_event_can_backfill_a_missing_base_id() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, None, 1_000));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 2_000));
        m.apply(&buff_remove(1, 417, 4_000));
        let snap = m.snapshot(5_000);
        assert_eq!(snap.rows[0].buffs[0].skill_id, 3_210_031);
        assert_eq!(snap.rows[0].buffs[0].damage, 3_000);
    }

    /// Two closed applications of the same buff accumulate uptime and
    /// apply count together, not overwrite.
    #[test]
    fn two_closed_applications_of_the_same_buff_accumulate() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 0));
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_remove(1, 417, 3_000)); // 2s
        m.apply(&buff_apply(1, 417, Some(3_210_031), 3_500));
        m.apply(&buff_remove(1, 417, 5_500)); // 2s
        let snap = m.snapshot(6_000);
        assert_eq!(snap.rows[0].buffs[0].damage, 4_000);
        assert_eq!(snap.rows[0].buffs[0].hits, 2);
        assert!((snap.rows[0].buffs[0].avg - 2_000.0).abs() < 0.01);
    }

    /// A buff on a uid with no row in the roster yet must not open one —
    /// mirrors `record_breakdowns`' rule for incoming damage/healing.
    #[test]
    fn a_buff_on_an_unrostered_uid_opens_no_row() {
        let mut m = Meter::new();
        m.apply(&buff_apply(1, 417, Some(3_210_031), 1_000));
        m.apply(&buff_remove(1, 417, 4_000));
        assert!(m.snapshot(5_000).rows.is_empty());
    }

    #[test]
    fn healing_lands_on_the_heal_tab_for_a_player_already_in_the_roster() {
        let mut m = Meter::new();
        // Open the row with real damage first — healing alone must not.
        m.apply(&dmg(1, 100, 1000));
        m.apply(&heal(1, 2, 55, 400, 1100));
        m.apply(&heal(1, 2, 55, 600, 1200));
        let snap = m.snapshot(2000);
        let row = &snap.rows[0];
        assert_eq!(row.heals.len(), 1);
        assert_eq!(row.heals[0].skill_id, 55);
        assert_eq!(row.heals[0].damage, 1000);
        assert_eq!(row.heals[0].hits, 2);
        // The share column divides by healing done, not damage done.
        assert!((row.heals[0].share_pct - 100.0).abs() < 0.01);
        // ...and none of it leaks into the damage view.
        assert_eq!(row.damage, 100);
        assert_eq!(row.skills.len(), 1);
    }

    #[test]
    fn healing_alone_never_opens_a_row() {
        let mut m = Meter::new();
        m.apply(&heal(1, 2, 55, 400, 1000));
        assert!(m.snapshot(2000).rows.is_empty());
    }

    #[test]
    fn damage_taken_lands_on_the_received_tab() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 1000));
        // A monster hitting the player: dropped by the damage pipeline's
        // `attacker_kind != Player` return, but it is exactly what the
        // received tab exists to show.
        m.apply(&hit_on_player(9, EntityKind::Monster, 1, 77, 250, 1100));
        m.apply(&hit_on_player(9, EntityKind::Monster, 1, 77, 250, 1200));
        let snap = m.snapshot(2000);
        let row = &snap.rows[0];
        assert_eq!(row.received.len(), 1);
        assert_eq!(row.received[0].skill_id, 77);
        assert_eq!(row.received[0].damage, 500);
        assert_eq!(row.received[0].hits, 2);
        assert!((row.received[0].share_pct - 100.0).abs() < 0.01);
        // The attacker was a monster, so no second row was opened for it.
        assert_eq!(snap.rows.len(), 1);
    }

    #[test]
    fn healing_received_lands_on_the_received_tab_too() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 1000));
        m.apply(&dmg(2, 100, 1000));
        m.apply(&heal(2, 1, 55, 300, 1100));
        let snap = m.snapshot(2000);
        let healed = snap.rows.iter().find(|r| r.uid == 1).expect("uid 1");
        assert_eq!(healed.received.len(), 1);
        assert_eq!(healed.received[0].skill_id, 55);
        assert_eq!(healed.received[0].damage, 300);
        // ...and the healer's own received tab stays empty.
        let healer = snap.rows.iter().find(|r| r.uid == 2).expect("uid 2");
        assert!(healer.received.is_empty());
    }

    #[test]
    fn the_dealt_tab_merges_outgoing_damage_and_healing() {
        let mut m = Meter::new();
        // Skill 10 damages, skill 55 heals, skill 20 does both.
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            target_uid: 9,
            target_kind: EntityKind::Monster,
            skill_id: 10,
            value: 700,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            target_uid: 9,
            target_kind: EntityKind::Monster,
            skill_id: 20,
            value: 100,
            timestamp_ms: 1100,
            ..Default::default()
        }));
        m.apply(&heal(1, 2, 20, 200, 1200));
        m.apply(&heal(1, 2, 55, 50, 1300));
        let snap = m.snapshot(2000);
        let row = &snap.rows[0];

        let dealt: HashMap<i32, &SkillRow> = row.dealt.iter().map(|r| (r.skill_id, r)).collect();
        assert_eq!(dealt.len(), 3);
        assert_eq!(dealt[&10].damage, 700);
        assert_eq!(dealt[&20].damage, 300);
        assert_eq!(dealt[&20].hits, 2);
        assert_eq!(dealt[&55].damage, 50);
        // Shares divide by everything dealt: 700 + 300 + 50 = 1050.
        assert!((dealt[&10].share_pct - 700.0 / 1050.0 * 100.0).abs() < 0.01);
        // Rows arrive amount-descending, like every other breakdown.
        assert_eq!(
            row.dealt.iter().map(|r| r.skill_id).collect::<Vec<_>>(),
            vec![10, 20, 55]
        );
        // The Dps tab is untouched by the merge.
        assert_eq!(row.skills.len(), 2);
    }

    #[test]
    fn a_missed_heal_counts_as_a_use_but_adds_no_amount() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 1000));
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            target_uid: 2,
            target_kind: EntityKind::Player,
            skill_id: 55,
            value: 400,
            is_heal: true,
            is_miss: true,
            timestamp_ms: 1100,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        let row = &snap.rows[0];
        assert_eq!(row.heals.len(), 1);
        assert_eq!(row.heals[0].hits, 1);
        assert_eq!(row.heals[0].damage, 0);
    }

    #[test]
    fn a_crit_heal_feeds_the_heal_tabs_crit_columns() {
        let mut m = Meter::new();
        m.apply(&dmg(1, 100, 1000));
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            target_uid: 2,
            target_kind: EntityKind::Player,
            skill_id: 55,
            value: 900,
            is_heal: true,
            crit: true,
            timestamp_ms: 1100,
            ..Default::default()
        }));
        m.apply(&heal(1, 2, 55, 100, 1200));
        let snap = m.snapshot(2000);
        let heal_row = &snap.rows[0].heals[0];
        assert_eq!(heal_row.crit_hits, 1);
        assert_eq!(heal_row.max_crit, 900);
        assert!((heal_row.crit_pct - 50.0).abs() < 0.01);
        assert!((heal_row.avg_crit - 900.0).abs() < 0.01);
        assert!((heal_row.avg_white - 100.0).abs() < 0.01);
        assert!((heal_row.avg - 500.0).abs() < 0.01);
    }

    #[test]
    fn merging_skill_stats_maxes_the_crit_rather_than_summing_it() {
        let mut a = SkillStats {
            total_damage: 100,
            hits: 1,
            crit_hits: 1,
            crit_damage: 100,
            max_crit: 100,
            ..Default::default()
        };
        let b = SkillStats {
            total_damage: 40,
            hits: 1,
            crit_hits: 1,
            crit_damage: 40,
            max_crit: 40,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.total_damage, 140);
        assert_eq!(a.hits, 2);
        assert_eq!(a.crit_hits, 2);
        assert_eq!(a.crit_damage, 140);
        assert_eq!(a.max_crit, 100);
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

    // -- issue #16: per-skill breakdown -------------------------------------

    fn skill_dmg(
        attacker_uid: i64,
        skill_id: i32,
        value: i64,
        crit: bool,
        ts: u64,
    ) -> ProtocolEvent {
        ProtocolEvent::Damage(DamageEvent {
            attacker_uid,
            attacker_kind: EntityKind::Player,
            skill_id,
            value,
            crit,
            timestamp_ms: ts,
            ..Default::default()
        })
    }

    #[test]
    fn avg_white_is_zero_when_every_hit_crits() {
        let mut m = Meter::new();
        m.apply(&skill_dmg(1, 42, 100, true, 1000));
        m.apply(&skill_dmg(1, 42, 200, true, 2000));
        let snap = m.snapshot(3000);
        let skill = &snap.rows[0].skills[0];
        assert_eq!(skill.hits, 2);
        assert_eq!(skill.crit_hits, 2);
        assert_eq!(skill.avg_white, 0.0);
    }

    #[test]
    fn a_lucky_non_crit_hit_counts_as_a_white_hit() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            skill_id: 7,
            value: 150,
            crit: false,
            lucky: true,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        let skill = &snap.rows[0].skills[0];
        assert_eq!(skill.hits, 1);
        assert_eq!(skill.crit_hits, 0);
        // A lucky non-crit hit is still a white hit (D6) — the average is
        // not zeroed just because `lucky` is set.
        assert!((skill.avg_white - 150.0).abs() < 0.001);
    }

    #[test]
    fn max_crit_tracks_the_largest_crit_only() {
        let mut m = Meter::new();
        m.apply(&skill_dmg(1, 9, 500, false, 1000));
        m.apply(&skill_dmg(1, 9, 300, true, 2000));
        m.apply(&skill_dmg(1, 9, 700, true, 3000));
        let snap = m.snapshot(4000);
        let skill = &snap.rows[0].skills[0];
        // The 500 non-crit hit must never be considered even though it's
        // the largest raw value — max_crit only ever looks at crit hits.
        assert_eq!(skill.max_crit, 700);
    }

    #[test]
    fn a_missed_swing_bumps_per_skill_hits_but_not_damage() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            skill_id: 3,
            value: 999,
            is_miss: true,
            timestamp_ms: 1000,
            ..Default::default()
        }));
        let snap = m.snapshot(2000);
        let skill = &snap.rows[0].skills[0];
        assert_eq!(skill.hits, 1);
        assert_eq!(skill.damage, 0);
    }

    #[test]
    fn per_skill_hits_sum_to_the_player_hit_count() {
        let mut m = Meter::new();
        m.apply(&skill_dmg(1, 1, 100, false, 1000));
        m.apply(&skill_dmg(1, 1, 100, false, 1500));
        m.apply(&skill_dmg(1, 2, 200, true, 2000));
        m.apply(&ProtocolEvent::Damage(DamageEvent {
            attacker_uid: 1,
            attacker_kind: EntityKind::Player,
            skill_id: 2,
            value: 999,
            is_miss: true,
            timestamp_ms: 2500,
            ..Default::default()
        }));
        let snap = m.snapshot(3000);
        let row = &snap.rows[0];
        let skill_hit_sum: u64 = row.skills.iter().map(|s| s.hits).sum();
        assert_eq!(skill_hit_sum, row.hits);
    }

    #[test]
    fn hits_per_minute_uses_the_snapshot_dps_window() {
        let mut m = Meter::new();
        // fight_start_ms = 0 (first event), last_event_ms = 25_000 (last
        // event, i=5) -> dps_duration_ms = 25_000ms. 6 hits over 25s is
        // 14.4 hits/min — the same denominator `Meter::snapshot` uses for
        // the row's own DPS, per D8.
        for i in 0..6 {
            m.apply(&skill_dmg(1, 5, 10, false, i * 5_000));
        }
        let snap = m.snapshot(40_000);
        let skill = &snap.rows[0].skills[0];
        assert!((skill.hits_per_min - 14.4).abs() < 0.001);
    }

    #[test]
    fn player_info_after_damage_renames_row() {
        let mut m = Meter::new();
        m.apply(&dmg(5, 100, 1000));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(5, EntityKind::Player),
            uid: 5,
            name: Some("Foo".to_string()),
            class: Some(Class::Stormblade),
            ability_score: None,
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].name, "Foo");
        assert_eq!(snap.rows[0].class, Some(Class::Stormblade));
    }

    fn player_info(uid: i64, name: &str) -> ProtocolEvent {
        ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(uid, EntityKind::Player),
            uid,
            name: Some(name.to_string()),
            class: Some(Class::Stormblade),
            ability_score: None,
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(7, EntityKind::Player),
            uid: 7,
            name: None,
            class: None,
            ability_score: Some(45_000),
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].ability_score, Some(45_000));
    }

    #[test]
    fn ability_score_survives_reset_like_name_and_class() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(3, EntityKind::Player),
            uid: 3,
            name: Some("Foo".to_string()),
            class: None,
            ability_score: Some(1000),
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(9, EntityKind::Player),
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), None]),
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(9, EntityKind::Player),
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), Some(102640)]),
            imagine_tiers: None,
        }));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(9, EntityKind::Player),
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(9, EntityKind::Player),
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([Some(3905), Some(102640)]),
            imagine_tiers: None,
        }));
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(9, EntityKind::Player),
            uid: 9,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: Some([None, None]),
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(8, EntityKind::Player),
            uid: 8,
            name: None,
            class: None,
            ability_score: None,
            season_strength: Some(12_345),
            imagines: None,
            imagine_tiers: None,
        }));
        let snap = m.snapshot(2000);
        assert_eq!(snap.rows[0].season_strength, Some(12_345));
    }

    #[test]
    fn season_strength_survives_reset_like_name_and_class() {
        let mut m = Meter::new();
        m.apply(&ProtocolEvent::Player(PlayerInfo {
            entity: EntityId::from_display_uid(4, EntityKind::Player),
            uid: 4,
            name: Some("Foo".to_string()),
            class: None,
            ability_score: None,
            season_strength: Some(999),
            imagines: None,
            imagine_tiers: None,
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
            entity: EntityId::from_display_uid(10, EntityKind::Monster),
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

        /// Issue #254: the death packet's timestamp opens the interval and
        /// this player's next acted event closes it — the only revive
        /// evidence this decoder gets (`PlayerStats::alive`).
        #[test]
        fn a_death_and_the_next_action_bound_one_dead_interval() {
            let mut m = Meter::new();
            m.apply(&dmg(2, 100, 1_000));
            m.apply(&death_hit(1, 2, 2_000));
            m.apply(&dmg(2, 100, 9_000));
            let snap = m.snapshot(10_000);
            let row = snap.rows.iter().find(|r| r.uid == 2).unwrap();
            assert_eq!(row.dead_ms, Some(7_000));
        }

        /// Two deaths in one pull sum, rather than the second overwriting
        /// the first.
        #[test]
        fn multiple_deaths_in_one_encounter_sum_their_intervals() {
            let mut m = Meter::new();
            m.apply(&dmg(2, 100, 1_000));
            m.apply(&death_hit(1, 2, 2_000));
            m.apply(&dmg(2, 100, 5_000));
            m.apply(&death_hit(1, 2, 10_000));
            m.apply(&dmg(2, 100, 12_500));
            let snap = m.snapshot(13_000);
            let row = snap.rows.iter().find(|r| r.uid == 2).unwrap();
            assert_eq!(row.deaths, 2);
            assert_eq!(row.dead_ms, Some(3_000 + 2_500));
        }

        /// A player still down when the snapshot is taken accrues up to the
        /// snapshot's clock — the live half of the "open death" rule (the
        /// frozen half is `a_death_open_at_the_wipe_counts_up_to_the_freeze`
        /// in the wipe tests).
        #[test]
        fn a_death_with_no_revive_yet_counts_up_to_the_snapshot_clock() {
            let mut m = Meter::new();
            m.apply(&dmg(2, 100, 1_000));
            m.apply(&death_hit(1, 2, 2_000));
            let row = |snap: &Snapshot| snap.rows.iter().find(|r| r.uid == 2).unwrap().dead_ms;
            assert_eq!(row(&m.snapshot(6_000)), Some(4_000));
            assert_eq!(
                row(&m.snapshot(9_000)),
                Some(7_000),
                "an open death ticks with the clock, like the fight timer"
            );
        }

        /// A retransmitted death packet is debounced out of the *count*;
        /// it must not quietly move the open interval's start forward
        /// either (`PlayerStats::dead_since_ms`).
        #[test]
        fn a_duplicate_death_packet_does_not_shorten_the_dead_interval() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 2_000));
            m.apply(&death_hit(1, 2, 2_000 + DEATH_DEBOUNCE_MS - 1));
            m.apply(&dmg(2, 100, 9_000));
            let snap = m.snapshot(10_000);
            let row = snap.rows.iter().find(|r| r.uid == 2).unwrap();
            assert_eq!(row.deaths, 1);
            assert_eq!(row.dead_ms, Some(7_000));
        }

        /// `set_alive`'s clamp: a hit retransmitted *behind* the death it
        /// preceded is not a battle rez, so it neither ends the interval
        /// nor rewinds it.
        #[test]
        fn an_action_older_than_the_death_does_not_end_the_dead_interval() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 5_000));
            m.apply(&dmg(2, 100, 4_000));
            let snap = m.snapshot(9_000);
            let row = snap.rows.iter().find(|r| r.uid == 2).unwrap();
            assert_eq!(row.dead_ms, Some(4_000), "still down, counting from 5_000");
        }

        /// Zero, not "unmeasured": the meter watched this player the whole
        /// pull and they never hit the floor. `None` is reserved for rows
        /// replayed out of the history database.
        #[test]
        fn a_player_who_never_died_reports_zero_dead_time() {
            let mut m = Meter::new();
            m.apply(&dmg(2, 100, 1_000));
            let snap = m.snapshot(9_000);
            assert_eq!(snap.rows[0].dead_ms, Some(0));
        }

        #[test]
        fn reset_clears_dead_time() {
            let mut m = Meter::new();
            m.apply(&death_hit(1, 2, 1_000));
            m.reset(ResetReason::Manual, 2_000);
            m.apply(&dmg(2, 50, 3_000));
            let snap = m.snapshot(9_000);
            assert_eq!(
                snap.rows.iter().find(|r| r.uid == 2).unwrap().dead_ms,
                Some(0)
            );
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
                entity: EntityId::from_display_uid(5, EntityKind::Player),
                uid: 5,
                name: Some("Fresh".to_string()),
                class: Some(Class::FrostMage),
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
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
                entity: EntityId::from_display_uid(5, EntityKind::Player),
                uid: 5,
                name: Some("Renamed".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
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
                entity: EntityId::from_display_uid(5, EntityKind::Player),
                uid: 5,
                name: Some("Ren".to_string()),
                class: Some(Class::Stormblade),
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&dmg(5, 100, 1000));

            // A simulated Imagine-transform packet: profession id decoded to
            // no class at all (see `bpsr_protocol::pb::class_of_profession_id`).
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(5, EntityKind::Player),
                uid: 5,
                name: None,
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
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
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("A".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(2, EntityKind::Player),
                uid: 2,
                name: Some("B".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(3, EntityKind::Player),
                uid: 3,
                name: Some("C".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            // Re-touch uid 1 so it becomes the most recently used, ahead of
            // 3 and 2 (in that order).
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("A".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
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
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("First".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(2, EntityKind::Player),
                uid: 2,
                name: Some("Second".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));

            let saved = m.names_for_save();
            assert_eq!(saved[0].0, 2);
            assert_eq!(saved[1].0, 1);
        }

        #[test]
        fn server_change_reset_preserves_names_for_save() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: 1000 });

            let saved = m.names_for_save();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].0, 1);
        }
    }

    /// Issue #201: the curated scene -> final-boss table
    /// (`tables::scene_final_boss`, generated from
    /// `crates/meter/data/SceneFinalBosses.json`) replaced issue #131's
    /// runtime learning. It covers only dungeons with a *single* boss, and it
    /// only supplies the caption before — or without — a boss hit: the
    /// hit-based lock in `recompute_boss` is untouched and still outranks it
    /// in `encounter_title`.
    mod scene_final_boss {
        use super::*;

        /// A curated entry: scene 1154 ("Unstable - Towering Ruin") ->
        /// monster 1152 ("Kartgriff").
        const CURATED_SCENE: u32 = 1154;
        const CURATED_BOSS: &str = "Kartgriff";
        /// 1001 ("Tina's Mindrealm") *is* a dungeon scene, but the curated
        /// table does not cover it — the "known dungeon, unknown boss" case.
        const UNCURATED_SCENE: u32 = 1001;

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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        #[test]
        fn a_curated_dungeon_names_its_final_boss_before_any_hit_lands() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: CURATED_SCENE,
            });

            let snap = m.snapshot(1_000);
            assert_eq!(snap.encounter.scene_boss_name, Some(CURATED_BOSS));
            assert!(!snap.encounter.is_boss, "nothing has been engaged yet");
        }

        #[test]
        fn an_uncurated_dungeon_names_no_boss_before_any_hit_lands() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: UNCURATED_SCENE,
            });

            assert_eq!(m.snapshot(1_000).encounter.scene_boss_name, None);
        }

        #[test]
        fn engaging_a_boss_in_an_uncurated_dungeon_teaches_the_table_nothing() {
            // The whole point of issue #201: no runtime learning survives, so
            // a boss fought here leaves no remembered caption behind once the
            // encounter is reset back to "nothing engaged".
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: UNCURATED_SCENE,
            });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 103, 0));
            m.reset(ResetReason::Manual, 1_000);

            assert_eq!(m.snapshot(2_000).encounter.scene_boss_name, None);
        }

        #[test]
        fn a_curated_entry_survives_a_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: CURATED_SCENE,
            });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 103, 0));
            m.reset(ResetReason::Manual, 1_000);

            assert_eq!(
                m.snapshot(2_000).encounter.scene_boss_name,
                Some(CURATED_BOSS)
            );
        }

        #[test]
        fn the_live_boss_lock_is_unaffected_by_the_curated_entry() {
            // Maintainer guidance for issue #201: the existing hit-based lock
            // stays exactly as it was. A recognized boss actually engaged in a
            // curated scene still populates `boss_name`/`is_boss`, which is
            // what `encounter_title` prefers over `scene_boss_name`.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: CURATED_SCENE,
            });
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 103, 0));

            let snap = m.snapshot(1_000);
            assert!(snap.encounter.is_boss);
            assert_eq!(snap.encounter.boss_name, Some("Ignisor"));
            assert_eq!(snap.encounter.scene_boss_name, Some(CURATED_BOSS));
        }

        #[test]
        fn an_open_world_scene_names_no_boss() {
            // Scene 8 ("Asterleeds") is not a dungeon at all, so it can never
            // carry a curated entry.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });

            assert_eq!(m.snapshot(1_000).encounter.scene_boss_name, None);
        }

        // -- curated multi-boss scenes (issue #150) -------------------------

        #[test]
        fn a_curated_multi_boss_raid_scene_never_guesses_a_boss_before_one_is_engaged() {
            // Scene 13023 ("Purge! Field of Forgotten Illusions") is a raid:
            // the party selects one of three bosses, so no single caption is
            // right before one is engaged. Engaging one still names it;
            // standing there having selected nothing — on entry, or after a
            // win or a wipe, both of which put the player back at the
            // selection without leaving the scene — must not.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: 13_023,
            });
            assert!(m.snapshot(0).encounter.multi_boss_scene);
            assert_eq!(m.snapshot(0).encounter.scene_boss_name, None);

            m.apply(&boss_hit(10, 1000));
            m.apply(&hp(10, 103_309, 1000));
            let snap = m.snapshot(2000);
            assert!(snap.encounter.is_boss);
            assert_eq!(
                snap.encounter.boss_name,
                Some("Paradox-Calamity Remnant - Final")
            );

            // Back at the selection: the boss just fought must not come back
            // as the header's answer while nothing is engaged.
            m.reset(ResetReason::Manual, 3000);
            let snap = m.snapshot(4000);
            assert!(snap.encounter.multi_boss_scene);
            assert_eq!(snap.encounter.scene_boss_name, None);
        }

        #[test]
        fn a_curated_single_boss_dungeon_is_not_treated_as_offering_a_boss_choice() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: CURATED_SCENE,
            });

            assert!(!m.snapshot(1_000).encounter.multi_boss_scene);
        }
    }

    mod reset {
        use super::*;

        /// "Ignisor" (103), a recognized boss: `is_boss_monster` is what
        /// gates the rollback reset (issue #157), so an enemy driving one
        /// has to be one.
        const BOSS: u32 = 103;
        /// "Golden Nappo": named but `MonsterType == 0`, i.e. a trash add.
        const TRASH: u32 = 10_900;

        fn hp(uid: i64, curr: u64, max: u64, ts: u64) -> ProtocolEvent {
            identified(uid, curr, max, BOSS, ts)
        }

        fn identified(uid: i64, curr: u64, max: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
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

        /// PR #223 review, finding 2: the rollback reset must survive the
        /// sync that *hands* `boss_entity` to the boss doing the rolling back.
        ///
        /// Two recognized bosses pulled together, both known only by
        /// `curr_hp` (issue #76's mid-pull join), so the ranking is a raw HP
        /// comparison and the leader changes whenever their bars cross. The
        /// wipe arrives as one `EnemyHp` that does two things at once: it
        /// completes B's 20% -> 100% rollback shape *and* lifts B's bar over
        /// A's, promoting B to `boss_entity` inside the very call that has to
        /// notice the rollback. Gating the rollback on the pre-`recompute_boss`
        /// capture the boss-death latch needs (issue #210/#211) made this
        /// return `None`: B was not the tracked boss when the sync landed,
        /// only immediately afterwards.
        #[test]
        fn rollback_fires_when_the_refill_itself_promotes_the_boss() {
            /// A second recognized boss, so both sides of the pair clear the
            /// `is_boss_monster` gate and only their HP separates them.
            const OTHER_BOSS: u32 = 102_801;

            fn curr_only(uid: i64, curr: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
                ProtocolEvent::EnemyHp(EnemyHp {
                    entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                    uid,
                    curr_hp: Some(curr),
                    max_hp: None,
                    monster_id: Some(monster_id),
                    timestamp_ms: ts,
                })
            }

            let mut m = Meter::new();
            // A is the bigger bar, so it holds `boss_entity` throughout the
            // pull...
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.apply(&curr_only(10, 1_000, BOSS, 0)), None);
            // ...while B is engaged alongside it and burned to 20% of its
            // own peak.
            m.apply(&boss_hit(11, 10));
            assert_eq!(m.apply(&curr_only(11, 500, OTHER_BOSS, 10)), None);
            assert_eq!(m.apply(&curr_only(11, 100, OTHER_BOSS, 100)), None);
            assert_eq!(m.boss_entity, Some(ek(10)));

            // The wipe: B's bar snaps back past its peak in a single sync,
            // which is both the rollback signature and the moment B outranks
            // A.
            let r = m.apply(&curr_only(11, 2_000, OTHER_BOSS, 200));
            assert_eq!(r, Some(ResetReason::BossHpRollback));
            // The reset clears the board, `boss_entity` included, so B's
            // promotion is only observable through the reset having fired
            // at all.
            assert_eq!(m.snapshot(1_000).total_damage, 0);
        }

        #[test]
        fn rollback_100_to_55_to_100_triggers_once() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.apply(&hp(10, 100, 100, 0)), None);
            assert_eq!(m.apply(&hp(10, 55, 100, 100)), None);
            let r = m.apply(&hp(10, 100, 100, 200));
            assert_eq!(r, Some(ResetReason::BossHpRollback));
        }

        #[test]
        fn rollback_100_to_96_to_100_never_triggers() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            // 96% never dips below the 95% drop threshold.
            m.apply(&hp(10, 96, 100, 100));
            let r = m.apply(&hp(10, 100, 100, 200));
            assert_eq!(r, None);
        }

        #[test]
        fn two_rollbacks_within_cooldown_trigger_once() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            m.apply(&hp(10, 55, 100, 100));
            let first = m.apply(&hp(10, 100, 100, 200));
            assert_eq!(first, Some(ResetReason::BossHpRollback));

            // 500ms later (< 2000ms cooldown): a second drop/recover must not fire.
            m.apply(&hp(10, 55, 100, 300));
            let second = m.apply(&hp(10, 100, 100, 700));
            assert_eq!(second, None);
        }

        #[test]
        fn cooldown_suppressed_rollback_does_not_refire_after_cooldown_expires() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            m.apply(&hp(10, 55, 100, 100));
            let first = m.apply(&hp(10, 100, 100, 200));
            assert_eq!(first, Some(ResetReason::BossHpRollback));

            // Within cooldown (last_reset_ms=200, cooldown=2000): the same
            // drop/recover shape is observed but suppressed.
            m.apply(&hp(10, 55, 100, 300));
            let suppressed = m.apply(&hp(10, 100, 100, 700));
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

            assert_eq!(m1.boss_entity, Some(ek(10)));
            assert_eq!(m2.boss_entity, Some(ek(10)));
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
        fn reset_clears_boss_entity_so_stale_hp_packet_cannot_refire() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            assert_eq!(m.boss_entity, Some(ek(10)));

            m.reset(ResetReason::Manual, 1000);
            assert_eq!(m.boss_entity, None);

            // An HP packet for the old boss uid, arriving before any new
            // damage picks a new boss, must not be able to drive a reset off
            // the stale boss_entity.
            let r = m.apply(&hp(10, 55, 100, 1100));
            assert_eq!(r, None);
        }

        #[test]
        fn reset_clears_took_damage_on_all_enemies() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0));
            m.apply(&hp(10, 100, 100, 0));
            assert!(m.enemies[&ek(10)].took_damage);
            m.reset(ResetReason::Manual, 1000);
            assert!(!m.enemies[&ek(10)].took_damage);
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
            assert!(m.boss_entity.is_none());
        }

        // -- issue #157: trash must not hold the reset heuristic ---------

        #[test]
        fn an_add_does_not_hold_the_boss_target_while_a_recognized_boss_is_present() {
            // The shape from issue #157's log: a reset clears `took_damage`
            // on every enemy, so the next enemy hit wins `boss_entity`
            // outright — and party AoE lands on adds first.
            let mut m = Meter::new();
            m.apply(&identified(10, 1_000_000, 1_000_000, BOSS, 0));
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.boss_entity, Some(ek(10)));

            m.reset(ResetReason::NewFight, 1_000);
            m.apply(&identified(20, 50_000, 50_000, TRASH, 1_100));
            m.apply(&boss_hit(20, 1_200));

            assert_eq!(
                m.boss_entity,
                Some(ek(10)),
                "the recognized boss keeps the target, not the add"
            );
        }

        #[test]
        fn an_undamaged_boss_in_aoi_range_does_not_take_the_target_from_the_add() {
            // PR #163 review, finding 2: the fallback reaches for a boss
            // "actually still being fought", and a boss the party has never
            // touched — merely synced into the enemy map by an AOI
            // `EnemyHp` packet on the way past — is not one. Letting it win
            // `boss_entity` puts its name in the header and its bar on screen
            // while the party is fighting something else entirely.
            let mut m = Meter::new();
            m.apply(&identified(10, 1_000_000, 1_000_000, BOSS, 0));
            m.apply(&identified(20, 50_000, 50_000, TRASH, 100));
            m.apply(&boss_hit(20, 200));

            assert_eq!(
                m.boss_entity,
                Some(ek(20)),
                "the add actually being hit holds the target"
            );
            let snap = m.snapshot(300);
            assert_eq!(snap.encounter.boss_monster_id, Some(TRASH));
            assert!(!snap.encounter.is_boss);
            assert_eq!(snap.encounter.boss_name, None);
        }

        #[test]
        fn a_dead_boss_does_not_take_the_target_back_from_a_live_add() {
            // The fallback above only reaches for a boss that is still up:
            // once the boss is dead and the party has moved on to trash,
            // the trash is genuinely what is being fought.
            let mut m = Meter::new();
            m.apply(&identified(10, 1_000_000, 1_000_000, BOSS, 0));
            m.apply(&boss_hit(10, 0));
            m.apply(&identified(10, 0, 1_000_000, BOSS, 100));
            m.reset(ResetReason::NewFight, 1_000);

            m.apply(&identified(20, 50_000, 50_000, TRASH, 1_100));
            m.apply(&boss_hit(20, 1_200));
            assert_eq!(m.boss_entity, Some(ek(20)));
        }

        #[test]
        fn an_abandoned_boss_does_not_take_the_target_back_from_a_live_add() {
            // PR #163 re-review of finding 2: the boss never died — the
            // party gave up on it and pulled an unrelated pack minutes
            // later, elsewhere in the same scene. "Alive and once damaged"
            // stays true of that boss forever (`is_alive` counts a
            // never-observed death as living), so recency is the only thing
            // that separates it from issue #157's boss, whose `took_damage`
            // a mid-pull reset cleared moments ago.
            let mut m = Meter::new();
            m.apply(&identified(10, 1_000_000, 1_000_000, BOSS, 0));
            m.apply(&boss_hit(10, 0));
            assert_eq!(m.boss_entity, Some(ek(10)));
            m.reset(ResetReason::Manual, 1_000);

            let much_later = 5 * 60_000;
            m.apply(&identified(20, 50_000, 50_000, TRASH, much_later));
            m.apply(&boss_hit(20, much_later + 100));
            assert_eq!(
                m.boss_entity,
                Some(ek(20)),
                "the pack being fought now holds the target, not the boss \
                 the party walked away from"
            );
        }

        #[test]
        fn a_boss_still_being_fought_holds_the_target_through_an_add_phase() {
            // The near side of the same window, and the reason it cannot be
            // tightened to the idle timeout: a boss goes immune, the party
            // spends the phase on adds, and the boss is still what the
            // fight is about when it comes back. Well inside
            // `BOSS_ENGAGEMENT_WINDOW_MS`, unlike the case above.
            let mut m = Meter::new();
            m.apply(&identified(10, 1_000_000, 1_000_000, BOSS, 0));
            m.apply(&boss_hit(10, 0));
            m.reset(ResetReason::BossHpRollback, 1_000);

            m.apply(&identified(20, 50_000, 50_000, TRASH, 1_100));
            m.apply(&boss_hit(20, 30_000));
            assert_eq!(
                m.boss_entity,
                Some(ek(10)),
                "the boss the party is still on keeps the target"
            );
        }

        #[test]
        fn an_unrecognized_enemys_hp_rollback_never_resets_the_encounter() {
            // No recognized boss anywhere in the encounter, so the add
            // legitimately holds `boss_entity` — and its bar snapping back
            // must still not wipe everyone's numbers.
            let mut m = Meter::new();
            m.apply(&boss_hit(20, 0));
            m.apply(&identified(20, 100, 100, TRASH, 0));
            assert_eq!(m.boss_entity, Some(ek(20)));
            m.apply(&identified(20, 55, 100, TRASH, 100));
            assert_eq!(m.apply(&identified(20, 95, 100, TRASH, 200)), None);
            assert_eq!(m.snapshot(300).total_damage, 1);
        }

        #[test]
        fn manual_reset_keeps_name_cache_for_late_damage() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
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
        fn fight_end_ms_is_none_until_the_fight_ends() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.fight_end_ms(), None);
        }

        #[test]
        fn fight_end_ms_reports_the_latched_end() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.tick(1_000 + idle());
            assert_eq!(m.fight_end_ms(), Some(1_000));
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

        /// issue #191: entering a genuinely different dungeon must not
        /// leave the previous instance's roster on screen next to the new
        /// one — the `Scene` arm has to clear it itself rather than wait on
        /// `NewFight`, which only fires once real damage lands in the new
        /// instance.
        #[test]
        fn scene_change_to_a_different_dungeon_clears_the_held_roster() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 }); // Tina's Mindrealm
            m.apply(&dmg(1, 5_000, 0));
            assert_eq!(m.tick(100_000), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: 31101,
            }); // a different dungeon
            assert_eq!(reason, Some(ResetReason::SceneChanged));

            let snap = m.snapshot(100_100);
            assert!(
                snap.rows.is_empty(),
                "the old dungeon's roster must not linger into the new one"
            );
            assert_eq!(snap.total_damage, 0);
            assert_eq!(m.fight_state(100_100), FightState::Idle);
        }

        /// issue #78, preserved by #191: a scene sync that keeps reporting
        /// the *same* dungeon (a resend, an AOI refresh) is not a real
        /// transition, so the held fight's numbers must stay on screen for
        /// the user to screenshot.
        #[test]
        fn repeated_scene_event_for_the_same_dungeon_does_not_clear_the_held_roster() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&dmg(1, 5_000, 0));
            assert_eq!(m.tick(100_000), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            assert_eq!(reason, None, "a same-scene resync must not report a reset");

            let snap = m.snapshot(200_000);
            assert_eq!(
                snap.total_damage, 5_000,
                "issue #78's hold must survive a same-scene resync"
            );
            assert!(!snap.rows.is_empty());
            assert_eq!(m.fight_state(200_000), FightState::Ended);
        }

        /// issue #191: a scene change landing while the fight is still
        /// `Active` — the party zoned out mid-pull — must latch
        /// `fight_end_ms` to the last real damage, freezing the clock and
        /// keeping the accumulated stats, exactly as the `ServerChanged`
        /// arm does for a reconnect (issue #138). The clear is deliberately
        /// *not* done in the same call, even though the destination is a
        /// dungeon: the fight just latched has not had a tick to be
        /// observed as `Ended` and recorded, so it is left to the new
        /// dungeon's first hit and the ordinary `NewFight` reset.
        #[test]
        fn scene_change_mid_fight_latches_the_clock_and_keeps_the_stats() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 }); // Tina's Mindrealm
            m.apply(&dmg(1, 700, 0));
            m.apply(&boss_hit(10, 500, false));
            m.apply(&hp(10, 50, Some(103), 500));

            // Well inside the idle window: still active, not yet held.
            assert_eq!(m.fight_state(1_000), FightState::Active);
            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: 31101,
            }); // a different dungeon
            assert_eq!(
                reason, None,
                "a cut-short fight defers its clear to the next fight's first hit"
            );

            assert_eq!(m.fight_state(600_000), FightState::Ended);
            let snap = m.snapshot(600_000);
            assert_eq!(
                snap.total_damage, 800,
                "player totals must survive zoning out mid-pull"
            );
            assert!(!snap.rows.is_empty());
            assert_eq!(
                snap.duration_ms, 500,
                "the clock latches to the last real damage, not fight_start_ms drifting"
            );
            assert_eq!(m.scene_id, Some(31101));
            // issue #152: the numbers on screen are the cut-short pull's, so
            // the header keeps naming the fight they were fought in.
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
        }

        /// issue #293: a meter attached mid-instance has no `scene_id` at
        /// all yet when damage starts — `ENTER_SCENE` fired once, before
        /// the meter existed, and the only source left is
        /// `SyncContainerData`'s full-state push, which can land well
        /// after the pull is already underway. That first-ever `Scene`
        /// event must be read as "learn where we already are", not as
        /// "the party just zoned out mid-pull" (the case
        /// `scene_change_mid_fight_latches_the_clock_and_keeps_the_stats`
        /// above covers) — the instance hasn't changed, so the fight must
        /// keep running rather than getting cut short.
        #[test]
        fn scene_learned_mid_fight_does_not_cut_it_short() {
            let mut m = Meter::new();
            // No `Scene` event yet: `m.scene_id` is still `None` even
            // though damage — and a boss pull — are already in progress.
            m.apply(&dmg(1, 700, 0));
            m.apply(&boss_hit(10, 500, false));
            m.apply(&hp(10, 50, Some(103), 500));
            assert_eq!(m.scene_id, None);
            assert_eq!(m.fight_state(1_000), FightState::Active);

            // The scene id finally arrives, unchanged from what it always
            // was — this is a *learn*, not a transition.
            let reason = m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            assert_eq!(
                reason, None,
                "learning the scene mid-fight must not report a reset"
            );

            assert_eq!(m.scene_id, Some(1001));
            assert_eq!(
                m.fight_state(1_000),
                FightState::Active,
                "a fight already in progress must not be cut short just because \
                 the meter finally learned which instance it's in"
            );
            let snap = m.snapshot(1_000);
            assert_eq!(snap.total_damage, 800);
            assert!(!snap.rows.is_empty());
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
        }

        /// PR #198 review, finding 1: entering a new dungeon must not let
        /// the previous run's boss caption the new one. The old boss is
        /// still alive and was engaged well inside
        /// `BOSS_ENGAGEMENT_WINDOW_MS`, so if its `EnemyState` survives the
        /// transition, issue #157's fallback hands it the new run's first
        /// hit on an unrecognized add — putting the wrong boss in the
        /// header, on the HP bar and in recorded history, which is the very
        /// bleed-through issue #191 exists to stop.
        #[test]
        fn a_new_dungeon_does_not_inherit_the_previous_ones_living_boss() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 }); // Tina's Mindrealm
            m.apply(&dmg(1, 5_000, 1_000));
            m.apply(&boss_hit(10, 1_000, false));
            m.apply(&hp(10, 50, Some(103), 1_000));
            assert_eq!(m.snapshot(1_000).encounter.boss_monster_id, Some(103));

            // Out to town first, which latches the abandoned pull: the
            // dungeon entry below is then an ordinary `!cut_short`
            // transition, the one path that resets.
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 }); // Asterleeds
            assert_eq!(m.fight_state(2_000), FightState::Ended);

            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: 31101,
            }); // a different dungeon
            assert_eq!(reason, Some(ResetReason::SceneChanged));

            // The new run's first hit lands on an add the boss table does
            // not recognize, which is exactly what arms issue #157's
            // fallback — and it is still inside the engagement window of
            // the boss left standing in the last dungeon.
            m.apply(&boss_hit(20, 20_000, false));
            m.apply(&hp(20, 50, None, 20_000));

            assert!(
                !m.enemies.contains_key(&ek(10)),
                "the old instance's entities must not survive into the new one"
            );
            assert_eq!(
                m.snapshot(20_000).encounter.boss_monster_id,
                None,
                "the new dungeon must not inherit the old one's boss"
            );
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
            assert!(m.boss_entity.is_none());
            assert!(
                m.scene_id.is_none(),
                "the scene is unknown until the next EnterScene"
            );
            // issue #152: the live scene is gone, but the *snapshot* is a
            // held fight's snapshot — its header names the fight whose
            // frozen numbers are on the rows above, not the nothing the
            // meter currently knows about.
            assert_eq!(snap.encounter.scene_id, Some(7));
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
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
            // wipe/rollback must keep resetting exactly as before. The
            // target has to be a recognized boss for that — issue #157
            // gates the rollback on it, so trash can never reset the meter.
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 100, Some(103), 0));
            m.apply(&hp(10, 55, Some(103), 100));
            let reason = m.apply(&hp(10, 100, Some(103), 200));
            assert_eq!(reason, Some(ResetReason::BossHpRollback));
        }

        #[test]
        fn the_next_fight_after_a_boss_kill_clears_the_held_stats() {
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 0, false));
            m.apply(&hp(10, 50, Some(103), 0));
            m.apply(&boss_hit(10, 1_000, true));
            assert_eq!(m.fight_state(1_100), FightState::Ended);

            // Next pull, only a few seconds later — well inside the idle
            // window, but past `FightConfig::post_end_grace_ms` (so this is
            // a genuinely new pull, not a grace-window straggler) and the
            // fight already ended, so this starts a new one.
            let reason = m.apply(&dmg(1, 700, 3_500));
            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(4_500).total_damage, 700);
        }

        #[test]
        fn names_survive_the_new_fight_reset() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Player(PlayerInfo {
                entity: EntityId::from_display_uid(1, EntityKind::Player),
                uid: 1,
                name: Some("Foo".to_string()),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: None,
                imagine_tiers: None,
            }));
            m.apply(&dmg(1, 100, 0));
            m.apply(&dmg(1, 100, 100_000));
            assert_eq!(m.snapshot(101_000).rows[0].name, "Foo");
        }

        // -- issue #151: the idle timeout must not end a live pull --------

        /// Any `tables::is_dungeon_scene` id.
        const DUNGEON_SCENE: u32 = 1_001;
        /// "Ignisor", a recognized boss.
        const BOSS: u32 = 103;
        /// "Moonstrike": the other half of Dreambloom Ruins' Caprahorn
        /// pair, two recognized bosses spawned and fought together.
        const PAIRED_BOSS: u32 = 102_801;
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
            // still very much in progress. Kept inside `BOSS_ENGAGEMENT_WINDOW_MS`
            // (issue #210/#211): past that bound a boss nobody has touched
            // reads as abandoned, not as a lull, however long the fight has
            // otherwise been running.
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + 6 * idle()), FightState::Active);
            assert_eq!(m.tick(1_000 + 6 * idle()), FightState::Active);
        }

        #[test]
        fn the_idle_timeout_still_ends_a_pull_on_trash_in_a_dungeon() {
            let mut m = in_dungeon();
            m.apply(&hp(10, 50, Some(TRASH), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + idle()), FightState::Ended);
        }

        #[test]
        fn an_idle_lull_does_not_end_the_fight_while_a_world_boss_is_still_up() {
            // Issue #313: the same immunity/mechanic lull as the case
            // above, in a plain open-world zone rather than an instance.
            // The suppression used to require `in_dungeon_scene()`, so out
            // here the 9s clock ran unopposed and the party's next hit
            // wiped a pull still 41.8% from done. A recognized boss the
            // party is actively hitting is a pull in progress wherever it
            // is standing.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            assert_eq!(m.fight_state(1_000 + 6 * idle()), FightState::Active);
            assert_eq!(m.tick(1_000 + 6 * idle()), FightState::Active);
        }

        #[test]
        fn the_engagement_window_ends_a_boss_fight_outside_a_dungeon() {
            // The control for the case above, and what replaced the scene
            // check as the bound (issue #313): the hold is not unlimited
            // out in the world either. It lapses
            // `BOSS_ENGAGEMENT_WINDOW_MS` after the last hit — the same
            // release valve every dungeon pull has had since issue
            // #210/#211 — and the idle timeout takes it from there.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
            m.apply(&hp(10, 50, Some(BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));

            // Inside the window: still a pull.
            assert_eq!(m.fight_state(1_000 + 6 * idle()), FightState::Active);

            // Past it: the guard releases and the fight ends.
            let now = 1_000 + BOSS_ENGAGEMENT_WINDOW_MS + idle();
            assert_eq!(m.fight_state(now), FightState::Ended);
            assert_eq!(
                m.snapshot(now).duration_ms,
                1,
                "the bound ends the fight retroactively at the last hit, \
                 so holding it open fabricates no elapsed time"
            );
        }

        #[test]
        fn the_world_dominator_arena_boss_holds_the_pull_open() {
            // Issue #313 end to end: scene 7152 ("World Dominator") with
            // monster 3000063 ("Denvel"), the exact pair from the report.
            // The boss went invulnerable, the idle timeout ended the fight,
            // and the party's next hit fired `ResetReason::NewFight` and
            // cleared every row mid-pull.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: 7_152,
            });
            m.apply(&hp(1, 50, Some(3_000_063), 0));
            m.apply(&boss_hit(1, 1_000, false));

            let lull = 1_000 + idle() + 500;
            assert_eq!(m.fight_state(lull), FightState::Active);

            // ...so the hit that ends the lull resumes the pull instead of
            // destroying it.
            let reason = m.apply(&boss_hit(1, lull, false));
            assert_eq!(reason, None, "the pull is still the same pull");

            // And the header names the boss, rather than blanking on an
            // unrecognized id (the third defect in the same report).
            let snap = m.snapshot(lull);
            assert!(snap.encounter.is_boss);
            assert_eq!(snap.encounter.boss_name, Some("Denvel"));
        }

        #[test]
        fn the_second_of_a_boss_pair_holds_the_pull_open_after_the_first_dies() {
            // Dreambloom Ruins' Caprahorn spawns two recognized bosses of
            // equal HP and the party fights both at once, so `boss_entity` can
            // only ever name one of them. Neither the liveness gate nor the
            // boss-death latch may read "the boss" as "the selected one".
            let mut m = in_dungeon();
            m.apply(&hp(10, 60, Some(BOSS), 0));
            m.apply(&hp(11, 50, Some(PAIRED_BOSS), 0));
            m.apply(&boss_hit(10, 1_000, false));
            m.apply(&boss_hit(11, 1_100, false));

            // The first of the pair falls; its twin is still up. Checked
            // inside `BOSS_ENGAGEMENT_WINDOW_MS` of the twin's own last hit
            // (issue #210/#211) — past that bound an untouched boss reads as
            // abandoned rather than as a lull, pair or not.
            m.apply(&boss_hit(10, 2_000, true));
            assert_eq!(m.fight_end_ms, None, "the pull is not over");
            assert_eq!(m.fight_state(2_000 + 6 * idle()), FightState::Active);

            // ...and once the twin falls too, the fight ends on the kill.
            m.apply(&boss_hit(11, 3_000, true));
            assert_eq!(m.fight_state(3_100), FightState::Ended);
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
        fn leaving_the_scene_latches_a_held_boss_pull_at_its_last_hit() {
            // Issue #191, not the idle timeout: a fight still running when
            // the scene changes is latched right there by the `SceneChanged`
            // arm, timestamped at the last real damage. That is what ends
            // this pull — issue #313 removed `engaged_boss_still_up`'s scene
            // check entirely, and these assertions are unmoved by it.
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

        /// `FightConfig::post_end_grace_ms`: trailing packets (a DoT tick,
        /// a killing-blow retransmit, a buff close) that land just after a
        /// fight ends still count against it, without reopening or
        /// extending it — see `Meter::apply_damage_grace`.
        mod grace_window {
            use super::*;

            fn grace() -> u64 {
                FightConfig::default().post_end_grace_ms
            }

            /// A player's real (non-heal) hit on a *monster* target,
            /// distinct from the top-level `dmg` helper (which leaves
            /// `target_kind` at its `Unknown` default): `damage_in_post_
            /// end_grace` requires an already-`took_damage` monster target
            /// to tell a trailing DoT tick from an unrelated new pull (PR
            /// #144's straggling-add contract, which grace must not
            /// re-break), so these tests need a real one.
            fn monster_hit(
                attacker_uid: i64,
                target_uid: i64,
                value: i64,
                ts: u64,
            ) -> ProtocolEvent {
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

            #[test]
            fn a_dot_tick_inside_the_grace_window_is_counted() {
                let mut m = Meter::new();
                m.apply(&monster_hit(1, 10, 100, 1_000));
                assert_eq!(m.tick(1_000 + idle()), FightState::Ended);
                assert_eq!(m.fight_end_ms(), Some(1_000));

                // A DoT tick 500ms after the latched end, on the same
                // already-engaged target, well inside the 2s grace window.
                let reason = m.apply(&monster_hit(1, 10, 50, 1_000 + 500));

                assert_eq!(reason, None, "a grace-window hit must not reset the fight");
                assert_eq!(m.snapshot(1_000 + idle() + 1_000).total_damage, 150);
                assert_eq!(
                    m.fight_end_ms(),
                    Some(1_000),
                    "fight_end_ms must not move for a grace-window hit"
                );
                assert_eq!(m.fight_state(1_000 + idle() + 1_000), FightState::Ended);
            }

            #[test]
            fn a_hit_past_the_grace_window_still_starts_a_new_fight() {
                let mut m = Meter::new();
                m.apply(&dmg(1, 100, 1_000));
                assert_eq!(m.tick(1_000 + idle()), FightState::Ended);
                assert!(grace() < 3_000, "test assumes the 2s default grace");

                // A hit 3s after the end: outside the grace window, so the
                // pre-existing `NewFight` path applies unchanged.
                let reason = m.apply(&dmg(1, 50, 1_000 + 3_000));

                assert_eq!(reason, Some(ResetReason::NewFight));
                assert_eq!(m.fight_end_ms(), None);
                let snap = m.snapshot(1_000 + 3_000 + 1_000);
                assert_eq!(
                    snap.total_damage, 50,
                    "the old fight's damage must be gone, not added to"
                );
            }

            #[test]
            fn a_grace_window_hit_does_not_move_the_dps_denominator() {
                let mut m = Meter::new();
                m.apply(&monster_hit(1, 10, 5_000, 0));
                m.apply(&monster_hit(1, 10, 5_000, 5_000));
                m.tick(5_000 + idle());
                let before = m.snapshot(5_000 + idle());
                // The dps window (`snapshot`'s `dps_duration_ms`, seconds)
                // backed out of the pre-grace snapshot, so the assertion
                // below never has to reach into private state to pin it.
                let dps_duration_secs = before.total_damage as f64 / before.total_dps;

                m.apply(&monster_hit(1, 10, 1_000, 5_000 + 500));

                let after = m.snapshot(5_000 + idle());
                assert_eq!(
                    after.total_damage, 11_000,
                    "the grace-window hit must still count toward totals"
                );
                let expected_dps = after.total_damage as f64 / dps_duration_secs;
                assert!(
                    (after.total_dps - expected_dps).abs() < 0.01,
                    "the dps denominator (elapsed) must not move: got {}, expected {}",
                    after.total_dps,
                    expected_dps
                );
            }

            #[test]
            fn a_grace_window_hit_never_updates_the_player_alive_or_wipe_state() {
                // Belt-and-suspenders for `apply_damage_grace`'s doc
                // comment: a grace-window player death must not arm
                // `wipe_hold`, which would change what a later, genuinely
                // new fight's `withholds_after_wipe` sees.
                let mut m = Meter::new();
                m.apply(&dmg(1, 100, 1_000));
                m.tick(1_000 + idle());

                m.apply(&ProtocolEvent::Damage(DamageEvent {
                    attacker_uid: 2,
                    attacker_kind: EntityKind::Monster,
                    target_uid: 1,
                    target_kind: EntityKind::Player,
                    value: 9_999,
                    is_dead: true,
                    timestamp_ms: 1_000 + 500,
                    ..Default::default()
                }));

                // A hit well past the grace window resumes as an ordinary
                // `NewFight`, exactly as it would have with no wipe hold at
                // all — proving the grace-window death above did not latch
                // one.
                let reason = m.apply(&dmg(1, 10, 1_000 + 3_000));
                assert_eq!(reason, Some(ResetReason::NewFight));
            }

            #[test]
            fn a_cast_inside_the_grace_window_is_still_counted() {
                let mut m = Meter::new();
                m.apply(&dmg(1, 100, 1_000));
                m.tick(1_000 + idle());

                m.apply(&ProtocolEvent::Cast(CastEvent {
                    caster: EntityId::from_display_uid(1, EntityKind::Player),
                    caster_uid: 1,
                    skill_id: 1550,
                    timestamp_ms: 1_000 + 500,
                }));

                let snap = m.snapshot(1_000 + idle() + 1_000);
                assert_eq!(snap.rows[0].casts[0].hits, 1);
                assert_eq!(m.fight_end_ms(), Some(1_000));
            }

            #[test]
            fn a_buff_close_inside_the_grace_window_credits_its_uptime() {
                let mut m = Meter::new();
                m.apply(&dmg(1, 100, 1_000));
                m.apply(&ProtocolEvent::BuffApply {
                    host: EntityId::from_display_uid(1, EntityKind::Player),
                    host_uid: 1,
                    buff_uuid: 417,
                    base_id: Some(3_210_031),
                    adds_layer: false,
                    timestamp_ms: 1_000,
                });
                m.tick(1_000 + idle());

                // The buff closes 500ms into the grace window.
                m.apply(&ProtocolEvent::BuffRemove {
                    host: EntityId::from_display_uid(1, EntityKind::Player),
                    host_uid: 1,
                    buff_uuid: 417,
                    removes_layer: false,
                    timestamp_ms: 1_000 + 500,
                });

                let snap = m.snapshot(1_000 + idle() + 1_000);
                assert_eq!(snap.rows[0].buffs[0].damage, 500);
                assert_eq!(m.fight_end_ms(), Some(1_000));
            }
        }
    }

    /// Issue #154/#155: a party wipe is the *end of a pull*, not a reset.
    /// The attempt's rows freeze for review, and nothing that happens
    /// afterwards — the boss's HP bar refilling, adds swinging at corpses,
    /// a stray AoE tick on trash during the run-back — touches them until
    /// the party genuinely re-engages the boss.
    mod wipe {
        use super::*;

        /// Any `tables::is_dungeon_scene` id: a wipe hold is an instance
        /// thing (PR #163 review, finding 1).
        const RAID_SCENE: u32 = 1_001;
        /// An open-world zone — `tables::is_dungeon_scene` is false for it.
        const FIELD_SCENE: u32 = 7;
        /// Paradox-Calamity Remnant (Origin), a recognized boss.
        const BOSS: u32 = 103_108;
        /// "Golden Nappo": named but `MonsterType == 0`, i.e. a trash add.
        const TRASH: u32 = 10_900;
        /// A *second* dungeon instance, for the post-wipe bounce into a
        /// different scene (issue #202).
        const NEXT_SCENE: u32 = 31_101;
        /// The second dungeon's own recognized boss — a different template
        /// id from `BOSS`, so a hijacked header is visible in an assert.
        const NEXT_BOSS: u32 = 103;
        const BOSS_UID: i64 = 10;
        const NEXT_BOSS_UID: i64 = 20;
        const ADD_UID: i64 = 11;
        /// An ordinary mob out in the world.
        const MOB_UID: i64 = 12;

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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(curr),
                max_hp: Some(1_000_000),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// The boss landing a killing blow on a party member.
        fn killing_blow(target_uid: i64, ts: u64) -> ProtocolEvent {
            killing_blow_from(BOSS_UID, target_uid, ts)
        }

        /// Any monster landing a killing blow on a player.
        fn killing_blow_from(attacker_uid: i64, target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid,
                attacker_kind: EntityKind::Monster,
                target_uid,
                target_kind: EntityKind::Player,
                value: 9_999,
                is_dead: true,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// A player landing a killing blow on *themselves* — a reflected
        /// hit, or a self-damaging skill. Attacker and victim are one uid,
        /// which is the case `killing_blow_from` (always a monster
        /// attacker) cannot express.
        fn self_killing_blow(uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: uid,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Player,
                value: 9_999,
                is_dead: true,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// A player healing a party member — the only kind of outgoing
        /// event a pure support ever produces.
        fn heal(attacker_uid: i64, target_uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid,
                attacker_kind: EntityKind::Player,
                target_uid,
                target_kind: EntityKind::Player,
                value: 4_000,
                is_heal: true,
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

        /// Issue #254: nobody revives after a wipe, so the last death of
        /// the pull stays open forever. It is counted up to the fight's
        /// *end*, not to the caller's clock, so a held attempt's death time
        /// freezes with the rest of its numbers.
        #[test]
        fn a_death_open_at_the_wipe_counts_up_to_the_freeze() {
            let m = wiped();
            let dead = |snap: &Snapshot, uid: i64| {
                snap.rows.iter().find(|r| r.uid == uid).unwrap().dead_ms
            };
            let snap = m.snapshot(66_000);
            assert_eq!(dead(&snap, 1), Some(1_000), "down at 5_000, wipe at 6_000");
            assert_eq!(dead(&snap, 2), Some(0), "fell as the fight ended");
            let later = m.snapshot(120_000);
            assert_eq!(
                dead(&later, 1),
                Some(1_000),
                "a frozen attempt's death time must not keep ticking"
            );
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
            assert_eq!(
                snap.duration_ms, 1_000,
                "issue #204: the elapsed timer restarts at the re-pull, it does \
                 not carry the wiped attempt's 5s forward"
            );
            assert_eq!(m.fight_state(31_000), FightState::Active);
        }

        // -- issue #204: the hold must be releasable, not wedgeable --

        #[test]
        fn a_re_pull_that_never_resolves_as_a_boss_still_releases_the_hold() {
            let mut m = wiped();
            // The party runs back and opens up again, but nothing they hit
            // ever resolves as a recognized boss: the respawn came up under
            // a fresh uid whose `EnemyHp` never landed, so its `monster_id`
            // is unknown and `withholds_after_wipe` has nothing to say yes
            // to. Before issue #204 that wedged the hold permanently — every
            // hit after it dropped on the floor and the elapsed timer showed
            // the wiped attempt forever.
            let repull_ms = 6_000 + WIPE_HOLD_RELEASE_MS;
            let r = m.apply(&hit(1, 4_242, 700, repull_ms));
            assert_eq!(r, Some(ResetReason::NewFight));
            let snap = m.snapshot(repull_ms + 2_000);
            assert_eq!(snap.total_damage, 700, "the re-pull records normally");
            assert_eq!(
                snap.duration_ms, 2_000,
                "the elapsed timer restarts at the re-pull"
            );
            assert_eq!(m.fight_state(repull_ms + 2_000), FightState::Active);
        }

        #[test]
        fn trash_damage_inside_the_release_window_still_holds_the_wipe_stats() {
            // Issue #154's guarantee, unchanged: for as long as the attempt
            // is genuinely being held for review, an AoE clipping an add on
            // the run-back is not the next pull.
            let mut m = wiped();
            m.apply(&enemy_hp(ADD_UID, 50_000, TRASH, 10_000));
            let last_held_ms = 6_000 + WIPE_HOLD_RELEASE_MS - 1;
            for ts in (20_000..last_held_ms).step_by(5_000) {
                assert_eq!(m.apply(&hit(1, ADD_UID, 900, ts)), None);
            }
            assert_eq!(m.apply(&hit(1, ADD_UID, 900, last_held_ms)), None);
            let snap = m.snapshot(last_held_ms);
            assert_eq!(snap.total_damage, 10_000, "still the wiped attempt");
            assert_eq!(snap.duration_ms, 5_000, "still the wiped attempt's clock");
            assert_eq!(m.fight_state(last_held_ms), FightState::Ended);
        }

        #[test]
        fn the_release_window_alone_never_clears_the_wipe_stats() {
            // Time passing is not a re-pull: the release is armed by the
            // clock but only ever fired by *player* damage, so a boss
            // swinging at the corpses long past the window leaves the
            // attempt exactly where it froze.
            let mut m = wiped();
            for ts in (7_000..=6_000 + WIPE_HOLD_RELEASE_MS + 30_000).step_by(1_000) {
                m.apply(&monster_swing(1, ts));
            }
            let now = 6_000 + WIPE_HOLD_RELEASE_MS + 31_000;
            assert_eq!(m.fight_state(now), FightState::Ended);
            assert_eq!(m.snapshot(now).duration_ms, 5_000);
            assert_eq!(m.snapshot(now).total_damage, 10_000);
        }

        // -- PR #163 review, finding 1: the hold needs a boss to lift it --

        #[test]
        fn a_solo_death_to_a_field_mob_does_not_freeze_the_meter() {
            // `party_is_wiped` is satisfied by a solo player dying once, and
            // out in the world there is no recognized boss to re-engage — so
            // latching the hold there left the meter frozen, and dropping
            // every event that reached it, until the player zoned or reset
            // by hand.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: FIELD_SCENE,
            });
            m.apply(&player_info(1, "Alpha"));
            m.apply(&enemy_hp(MOB_UID, 50_000, TRASH, 0));
            m.apply(&hit(1, MOB_UID, 1_000, 1_000));
            m.apply(&killing_blow_from(MOB_UID, 1, 2_000));

            assert_eq!(
                m.fight_state(2_500),
                FightState::Active,
                "dying to a field mob is not a wipe worth freezing"
            );

            // ...and the fight goes on recording: the player gets back up
            // and finishes the thing off.
            let r = m.apply(&hit(1, MOB_UID, 2_000, 3_000));
            assert_eq!(r, None, "no reset — it is the same fight continuing");
            let snap = m.snapshot(3_500);
            assert_eq!(snap.total_damage, 3_000, "the second hit still counts");
            assert_eq!(snap.rows[0].hits, 2);
            assert_eq!(snap.rows[0].deaths, 1);
        }

        #[test]
        fn the_wipe_hold_still_requires_an_instance() {
            // Issue #313 widened `engaged_boss_still_up` past
            // `in_dungeon_scene()` for the *idle* path only. The wipe hold
            // keeps that gate at its own call site: PR #163 review finding
            // 1's reasoning is about instances specifically, and freezing
            // the meter out in the open world — where the only thing that
            // lifts a hold is a hit on a recognized boss — is how a solo
            // player ended up with a dead meter until they zoned.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: FIELD_SCENE,
            });
            m.apply(&player_info(1, "Alpha"));
            m.apply(&player_info(2, "Bravo"));
            m.apply(&enemy_hp(BOSS_UID, 500_000, BOSS, 0));
            m.apply(&hit(1, BOSS_UID, 1_000, 1_000));
            m.apply(&hit(2, BOSS_UID, 1_000, 1_500));
            m.apply(&killing_blow(1, 2_000));
            m.apply(&killing_blow(2, 2_500));

            assert_eq!(m.fight_end_ms, None, "no wipe latch outside an instance");
            assert!(!m.wipe_hold, "and no hold to have to lift");
            assert_eq!(m.fight_state(3_000), FightState::Active);
        }

        #[test]
        fn a_party_death_on_trash_inside_an_instance_does_not_freeze_the_meter() {
            // The same finding one room earlier: the party is in the
            // instance, but the pull is a trash pack, so a hold latched here
            // could only ever be lifted by walking to the boss.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: RAID_SCENE,
            });
            m.apply(&player_info(1, "Alpha"));
            m.apply(&player_info(2, "Bravo"));
            m.apply(&enemy_hp(ADD_UID, 50_000, TRASH, 0));
            m.apply(&hit(1, ADD_UID, 1_000, 1_000));
            m.apply(&hit(2, ADD_UID, 1_000, 1_500));
            m.apply(&killing_blow_from(ADD_UID, 1, 2_000));
            m.apply(&killing_blow_from(ADD_UID, 2, 2_500));

            assert_eq!(m.fight_state(3_000), FightState::Active);
            assert_eq!(m.apply(&hit(1, ADD_UID, 1_000, 3_000)), None);
            assert_eq!(m.snapshot(3_500).total_damage, 3_000);
        }

        #[test]
        fn a_scene_change_during_the_wipe_hold_defers_its_reset_too() {
            // Issue #202: the wipe already latched `fight_end_ms` (so
            // `cut_short` reads false), and a `Scene` packet with a
            // differing dungeon scene id can land before the next tick gets
            // a chance to observe the fight as `Ended` — e.g. bounced to a
            // checkpoint/lobby sub-map right after the party goes down. The
            // dungeon-transition reset guard must hold off on `wipe_hold`
            // exactly as it already does on `cut_short` (issue #154's
            // "don't destroy an unobserved fight" principle), instead of
            // clearing `players` (and its death counts) out from under it.
            let mut m = wiped();
            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: NEXT_SCENE,
            });
            assert_eq!(
                reason, None,
                "an unobserved wipe defers its clear to the next fight's first hit, same as cut_short"
            );

            assert_eq!(
                m.players.get(&pk(1)).map(|p| p.deaths),
                Some(1),
                "death counts must survive the post-wipe scene bounce"
            );
            assert_eq!(m.players.get(&pk(2)).map(|p| p.deaths), Some(1));

            assert_eq!(m.fight_state(60_000), FightState::Ended);
            let snap = m.snapshot(60_000);
            assert_eq!(
                snap.total_damage, 10_000,
                "player totals must survive the post-wipe scene bounce too"
            );
            assert_eq!(snap.rows.len(), 2);
        }

        #[test]
        fn the_next_dungeon_s_first_hp_sync_cannot_hijack_the_held_boss() {
            // PR #205 review, finding 1: the reset the test above withholds
            // used to take the `enemies.clear()` down with it, leaving the
            // departed instance's boss in the map — damaged, alive, and the
            // only candidate `rank_boss(|e| e.took_damage)` has. The new
            // dungeon's first `EnemyHp` packet then named *it* as the boss
            // of the scene the party had just walked into.
            let mut m = wiped();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: NEXT_SCENE,
            });

            // Nobody has attacked anything here yet; this is just the next
            // dungeon's boss syncing into AOI range.
            m.apply(&enemy_hp(NEXT_BOSS_UID, 800_000, NEXT_BOSS, 30_000));

            assert_ne!(
                m.boss_entity,
                Some(ek(BOSS_UID)),
                "the last dungeon's boss must not own the target in this one"
            );
            assert_eq!(
                m.boss_entity, None,
                "and nothing here has been engaged yet either"
            );

            // PR #209 removed the runtime scene->boss *learning* system
            // (`Meter::scene_bosses` and friends) this test used to guard
            // here: a stale `boss_entity` had no way to poison a *learned*
            // per-scene answer, because that answer no longer exists.
            // `EncounterInfo::scene_boss_name` (`Meter::snapshot`) is now a
            // pure lookup into the static, curated
            // `tables::SCENE_FINAL_BOSSES` keyed only on `scene_id` — it
            // cannot observe `boss_entity` or the live enemy map at all, so
            // there is nothing left for a cross-dungeon hijack to corrupt.

            // The frozen wipe display is untouched by all of that: it is
            // captioned by `fight_identity`, not by the live enemy map.
            let snap = m.snapshot(30_500);
            assert_eq!(snap.encounter.boss_monster_id, Some(BOSS));
            assert_eq!(snap.encounter.scene_id, Some(RAID_SCENE));
            assert_eq!(snap.total_damage, 10_000);
            assert_eq!(m.fight_state(30_500), FightState::Ended);
        }

        #[test]
        fn the_wipe_hold_survives_the_dungeon_transition_it_deferred() {
            // PR #205 review, finding 2: `wipe_hold` was dropped
            // unconditionally on any scene change, including the one whose
            // reset it had just withheld — so the very next trash hit
            // satisfied the `NewFight` gate and wiped the attempt the guard
            // had spent the whole call protecting.
            let mut m = wiped();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: NEXT_SCENE,
            });

            // Running in, an AoE clips an add — the exact event issue #154
            // exists to ignore.
            m.apply(&enemy_hp(ADD_UID, 50_000, TRASH, 30_000));
            let r = m.apply(&hit(1, ADD_UID, 900, 30_500));
            assert_eq!(r, None, "trash must not clear a held wipe, here either");
            assert_eq!(m.fight_state(31_000), FightState::Ended);
            let snap = m.snapshot(31_000);
            assert_eq!(snap.total_damage, 10_000);
            assert_eq!(snap.rows.len(), 2);
            assert_eq!(snap.encounter.boss_monster_id, Some(BOSS));

            // Re-engaging a recognized boss is still what lifts it — and the
            // deferred clear lands there, on the new instance's own pull.
            m.apply(&enemy_hp(NEXT_BOSS_UID, 800_000, NEXT_BOSS, 31_500));
            let r = m.apply(&hit(1, NEXT_BOSS_UID, 400, 32_000));
            assert_eq!(r, Some(ResetReason::NewFight));
            let snap = m.snapshot(32_500);
            assert_eq!(snap.total_damage, 400, "the next pull starts clean");
            assert_eq!(snap.encounter.boss_monster_id, Some(NEXT_BOSS));
            assert_eq!(m.fight_state(32_500), FightState::Active);
        }

        #[test]
        fn leaving_the_instance_ends_the_wipe_hold() {
            let mut m = wiped();
            // Zoning out to the world — no reconnect, just the next scene.
            // The attempt being held belongs to the instance being left, and
            // out here nothing the player hits will ever be the recognized
            // boss that lifts the hold.
            m.apply(&ProtocolEvent::Scene {
                level_map_id: FIELD_SCENE,
            });
            m.apply(&enemy_hp(MOB_UID, 50_000, TRASH, 30_000));
            let r = m.apply(&hit(1, MOB_UID, 300, 30_500));
            assert_eq!(r, Some(ResetReason::NewFight));
            assert_eq!(m.snapshot(31_000).total_damage, 300);
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

        // -- issue #212: "wiped" must mean "down right now", not "has died
        // at some point this pull" --

        #[test]
        fn a_staggered_rez_does_not_falsely_latch_a_wipe() {
            // `party_is_wiped` used to read `deaths > 0` per player — a
            // *cumulative* count for the whole attempt — so the moment the
            // last still-standing player took their first death, every row
            // read `deaths > 0` even though an earlier death had long since
            // been battle-rezzed and that player was back in the fight.
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            // Alpha gets rezzed and lands a hit — back in the fight.
            let r = m.apply(&hit(1, BOSS_UID, 100, 6_000));
            assert_eq!(r, None, "a rez mid-fight is not a new fight");
            assert_eq!(
                m.fight_state(6_500),
                FightState::Active,
                "one player down (and back up) is not a wipe"
            );

            // Bravo goes down too. Under the old cumulative rule every row
            // now has `deaths > 0`, but Alpha is alive and still swinging.
            m.apply(&killing_blow(2, 7_000));
            assert_eq!(
                m.fight_state(7_500),
                FightState::Active,
                "cumulative deaths across the pull must not read as a \
                 simultaneous wipe"
            );

            // Damage after that point must still be counted, not dropped on
            // the floor by a falsely-latched hold.
            let r = m.apply(&hit(1, BOSS_UID, 3_000, 8_000));
            assert_eq!(r, None);
            let snap = m.snapshot(8_500);
            assert_eq!(
                snap.total_damage,
                10_000 + 100 + 3_000,
                "post-false-wipe damage must not be dropped"
            );
            assert_eq!(m.fight_state(8_500), FightState::Active);
        }

        #[test]
        fn a_rez_followed_by_a_real_full_wipe_still_latches() {
            // The fix must not overcorrect into never latching once anyone
            // has ever died: a genuine full-party wipe after an earlier rez
            // still has to end and freeze the fight.
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            m.apply(&hit(1, BOSS_UID, 100, 6_000));
            assert_eq!(m.fight_state(6_500), FightState::Active);

            // Now the whole party goes down together, with nobody rezzed in
            // between.
            m.apply(&killing_blow(1, 7_000));
            m.apply(&killing_blow(2, 8_000));
            assert_eq!(
                m.fight_state(8_500),
                FightState::Ended,
                "a real full-party wipe after a rez must still latch"
            );
            let snap = m.snapshot(68_000);
            assert_eq!(snap.total_damage, 10_000 + 100);
            assert_eq!(
                snap.duration_ms, 7_000,
                "frozen at the second death (8_000) minus the first hit (1_000)"
            );
        }

        #[test]
        fn a_self_inflicted_killing_blow_leaves_the_player_down() {
            // PR #224 review, finding 1: the death write and the revive
            // write used to sit on either side of the same `apply_damage`
            // call, so an event whose attacker *is* its victim recorded
            // the death and then immediately un-recorded it. That player
            // read `alive` for the rest of the pull, and no wipe involving
            // them could ever latch.
            let mut m = pull();
            m.apply(&self_killing_blow(1, 5_000));
            assert_eq!(
                m.fight_state(5_500),
                FightState::Active,
                "one player down is not a wipe"
            );

            m.apply(&killing_blow(2, 6_000));
            assert_eq!(
                m.fight_state(6_500),
                FightState::Ended,
                "a player who killed themselves is still down for the wipe"
            );
        }

        #[test]
        fn a_rezzed_healer_who_only_ever_heals_is_not_a_corpse() {
            // PR #224 review, finding 2: the revive write sat below the
            // `is_heal` early return, so a player whose whole output is
            // heal-typed never reached it. One death and they stayed down
            // for the rest of the pull — the same false wipe issue #212 is
            // about, scoped to the roles that deal no damage.
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            // Alpha is battle-rezzed and goes back to healing. No damage
            // of their own, ever again.
            let r = m.apply(&heal(1, 2, 6_000));
            assert_eq!(r, None, "a heal is not a new fight");
            assert_eq!(m.fight_state(6_500), FightState::Active);

            // Bravo goes down. Alpha is up and casting, so the party is
            // not down together.
            m.apply(&killing_blow(2, 7_000));
            assert_eq!(
                m.fight_state(7_500),
                FightState::Active,
                "a healer who is casting is not a corpse"
            );

            // ...and the healer going down too still latches the wipe.
            m.apply(&killing_blow(1, 9_000));
            assert_eq!(m.fight_state(9_500), FightState::Ended);
        }

        #[test]
        fn a_stale_hit_behind_a_death_packet_is_not_a_rez() {
            // PR #224 review, finding 3: `alive` was a last-write-wins
            // bool with no clock on it, unlike every other order-sensitive
            // field here (`EnemyState::last_damaged_ms` and friends), so a
            // hit retransmitted *behind* the death packet it preceded
            // flipped the victim back up and the wipe went unnoticed.
            let mut m = pull();
            m.apply(&killing_blow(1, 5_000));
            // Alpha's last swing before dying, arriving late.
            m.apply(&hit(1, BOSS_UID, 100, 4_500));
            m.apply(&killing_blow(2, 6_000));
            assert_eq!(
                m.fight_state(6_500),
                FightState::Ended,
                "a hit older than the death it preceded cannot revive a corpse"
            );
        }

        /// A five-player instance roster, the boss burned to 20% — far
        /// enough below `hp_drop_below_pct` to arm the rollback detector —
        /// and 10_000 damage on the board (issue #259).
        fn five_player_pull() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: RAID_SCENE,
            });
            for uid in 1..=5 {
                m.apply(&player_info(uid, "Party"));
            }
            m.apply(&enemy_hp(BOSS_UID, 1_000_000, BOSS, 0));
            m.apply(&hit(1, BOSS_UID, 5_000, 1_000));
            m.apply(&hit(2, BOSS_UID, 5_000, 1_500));
            m.apply(&enemy_hp(BOSS_UID, 200_000, BOSS, 4_000));
            m
        }

        /// The boss's bar snapping back to full — the server giving up on
        /// the pull.
        fn rollback(ts: u64) -> ProtocolEvent {
            enemy_hp(BOSS_UID, 1_000_000, BOSS, ts)
        }

        /// Issue #259: whether a failed attempt reached the history
        /// database was luck. The two paths that can claim a lost pull —
        /// the death packet's wipe latch and the HP-rollback reset — were
        /// unordered, so the same boss in the same scene ended `cause=wipe`
        /// ten times on one raid night and vanished as
        /// `reset reason=BossHpRollback` 31 times on the next.
        ///
        /// The shape that loses the race: the roster is not unanimously
        /// down when the bar refills — here one of five is a straggler who
        /// never produced a death packet at all — so the death path's
        /// `party_is_wiped` never fires, and the rollback arrives to find a
        /// live fight it is entitled to throw away. `party_mostly_down`
        /// (four of five, the `WIPE_PARTY_DOWN_FRACTION` threshold) is what
        /// claims it as a wipe first, and the `held` test then defers the
        /// reset so the ended attempt survives long enough to be recorded.
        #[test]
        fn a_rollback_with_the_party_down_ends_as_a_wipe_instead_of_discarding_the_attempt() {
            let mut m = five_player_pull();
            for uid in 1..=4 {
                m.apply(&killing_blow(uid, 5_000 + uid as u64 * 100));
            }
            assert_eq!(
                m.fight_end_ms, None,
                "four of five down is not the unanimous wipe the death path demands, \
                 which is exactly why this case used to be lost"
            );

            let reason = m.apply(&rollback(6_000));

            assert_eq!(
                reason, None,
                "the attempt is ended and kept, not thrown away as a reset"
            );
            assert_eq!(
                m.fight_end_ms,
                Some(6_000),
                "latched as a wipe at the rollback"
            );
            assert!(m.wipe_hold, "held for review like any other wipe");
            assert_eq!(m.fight_state(6_500), FightState::Ended);
            assert_eq!(
                m.snapshot(6_500).total_damage,
                10_000,
                "the wiped attempt's rows survive for the history recorder to see"
            );
        }

        /// The control (issue #259): a rollback with the party still
        /// standing is not a wipe — it is the boss being abandoned,
        /// de-aggroed or re-pulled by someone else, which is the case the
        /// reset heuristic exists for. Two of five down is below
        /// `WIPE_PARTY_DOWN_FRACTION`, so nothing about the old behaviour
        /// changes here.
        #[test]
        fn a_rollback_with_most_of_the_party_still_up_still_resets() {
            let mut m = five_player_pull();
            m.apply(&killing_blow(1, 5_000));
            m.apply(&killing_blow(2, 5_100));

            let reason = m.apply(&rollback(6_000));

            assert_eq!(reason, Some(ResetReason::BossHpRollback));
            assert_eq!(m.fight_end_ms, None, "a reset, not a fight end");
            assert_eq!(m.snapshot(6_500).total_damage, 0);
        }
    }

    /// Issue #210/#211: a boss-select raid scene lets the party pick which
    /// of several final bosses to pull (`phase::is_boss_select_scene`).
    /// Killing the currently-engaged selection must end that fight exactly
    /// like an ordinary single-boss dungeon, and picking the next selection
    /// must start a brand-new fight rather than continuing the dead one's.
    mod boss_select_scene {
        use super::*;

        /// Scene 13023, "Purge! Field of Forgotten Illusions" — the raid
        /// issue #150 and issue #210 were both reported from
        /// (`tables::scene_name`, `phase::is_boss_select_scene`).
        const RAID_SCENE: u32 = 13_023;
        /// Paradox-Calamity Remnant - Origin: the raid's first selection.
        const ORIGIN: u32 = 103_108;
        /// Paradox-Calamity Remnant - Continuation: the second selection.
        const CONTINUATION: u32 = 103_208;

        fn idle() -> u64 {
            FightConfig::default().idle_timeout_ms
        }

        fn in_raid() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: RAID_SCENE,
            });
            m
        }

        fn hp(uid: i64, curr: u64, max: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// A player hit on monster `uid`, optionally the killing blow.
        fn hit(uid: i64, ts: u64, is_dead: bool) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 500,
                is_dead,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        #[test]
        fn killing_the_first_selection_ends_the_fight_and_the_next_selection_starts_fresh() {
            let mut m = in_raid();
            m.apply(&hp(10, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(10, 1_000, false));
            m.apply(&hit(10, 2_000, true));

            assert_eq!(
                m.fight_state(2_100),
                FightState::Ended,
                "the kill should end the fight immediately"
            );
            assert_eq!(
                m.fight_end_boss_id,
                Some(ORIGIN),
                "cause=boss_death: only end_fight_on_boss_death ever sets this"
            );

            // Selecting the next boss must start a brand-new fight, not
            // resume the dead one's numbers — Origin and Continuation are
            // deliberately not phase-grouped (issue #153).
            m.apply(&hp(11, 1_000_000, 1_000_000, CONTINUATION, 5_000));
            let reason = m.apply(&hit(11, 6_000, false));
            assert_eq!(reason, Some(ResetReason::NewFight));
            assert_eq!(
                m.snapshot(6_100).total_damage,
                500,
                "boss 2's damage must not include boss 1's"
            );
        }

        #[test]
        fn a_boss_never_seen_to_die_does_not_wedge_the_fight_open_forever() {
            // Issue #210/#211: production logs show a boss's death can
            // simply never be observed — no `is_dead` damage event and no
            // `EnemyHp` sync to 0, ever (a dropped TCP segment). The old
            // `engaged_boss_still_up` read a never-observed death as "alive"
            // permanently, which permanently suppressed the idle-timeout
            // fallback that is the only other way a fight can end.
            let mut m = in_raid();
            m.apply(&hp(10, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(10, 1_000, false));

            // Well past the idle timeout but still inside the boss
            // engagement window: a real mechanic/immunity lull, so the pull
            // must still read as active (issue #151).
            assert_eq!(
                m.fight_state(1_000 + 5 * idle()),
                FightState::Active,
                "a lull inside the engagement window must not end the fight"
            );

            // No death signal ever arrives. Once nobody has touched the
            // boss for longer than the engagement window, the idle timeout
            // must be allowed to reclaim the fight instead of hanging
            // forever.
            let far_later = 1_000 + BOSS_ENGAGEMENT_WINDOW_MS + idle() + 1;
            assert_eq!(m.fight_state(far_later), FightState::Ended);
            assert_eq!(
                m.fight_end_boss_id, None,
                "an idle-timeout end, not a boss-death end"
            );
        }

        #[test]
        fn a_selections_death_still_ends_the_fight_even_though_the_other_selection_was_engaged_long_ago_and_left_alone()
         {
            // Defect 2: `recompute_boss` ranks a living enemy above a dead
            // one, so once Origin is marked dead, `recompute_boss` moves
            // `boss_entity` onto the already-damaged, still-alive Continuation
            // *before* the `boss_entity == target_uid` guard below is checked
            // — unless that fact is captured first.
            //
            // Defect 3: even with the ordering fixed, `other_living_boss`
            // must not read Continuation as "another living boss" holding
            // the pull open just because it is the raid's other selection —
            // sequential play, issue #150/#210's raid shape, not Caprahorn's
            // concurrent pair.
            //
            // Continuation is deliberately engaged *once*, long enough ago
            // to fall outside `BOSS_ENGAGEMENT_WINDOW_MS` by the time Origin
            // dies, and never touched again — a single stray hit followed by
            // abandonment, not the sustained concurrent engagement the
            // co-engagement rule exists to recognize (see the Caprahorn test
            // below for that case). Origin is re-hit periodically, always
            // within the idle timeout of the previous event, so none of
            // those hits are themselves intercepted by the idle-timeout
            // preemption at the top of `apply_damage`.
            let mut m = in_raid();
            // Origin carries the bigger pool, so it is the tracked boss.
            m.apply(&hp(10, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hp(11, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hit(10, 1_000, false));
            m.apply(&hit(11, 1_100, false));
            assert_eq!(
                m.boss_entity,
                Some(ek(10)),
                "Origin, the larger pool, is tracked"
            );

            let step = idle() - 1_000; // comfortably inside the idle timeout
            let mut ts = 1_100;
            while ts <= 1_100 + BOSS_ENGAGEMENT_WINDOW_MS {
                ts += step;
                m.apply(&hit(10, ts, false));
            }
            let kill_ts = ts + step;
            m.apply(&hit(10, kill_ts, true));

            assert_eq!(
                m.fight_end_ms,
                Some(kill_ts),
                "Origin's death must still end the fight"
            );
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));
        }

        #[test]
        fn the_real_caprahorn_pair_holds_the_pull_open_in_its_own_boss_select_scene() {
            // Regression guard (issue #210 review): Dreambloom Ruins
            // (13011/13012/13013) is itself a `phase::is_boss_select_scene`
            // — the same kind of scene as Field of Forgotten Illusions — but
            // its Caprahorn selection spawns *two* equal-HP bosses fought
            // *concurrently* rather than sequentially
            // (`phase::BOSS_SELECT_SCENES`'s own doc comment). A blanket
            // "boss-select scene disables `other_living_boss`" fix would
            // end the fight the instant the first twin falls even though its
            // partner is still being actively hit — exactly the bug
            // `the_second_of_a_boss_pair_holds_the_pull_open_after_the_first_dies`
            // guards against, just inside a real boss-select scene instead
            // of a synthetic dungeon id.
            const DREAMBLOOM_SCENE: u32 = 13_011;
            /// "Ignisor", a recognized boss — stands in for one Caprahorn
            /// twin.
            const CAPRAHORN_A: u32 = 103;
            /// "Moonstrike", the other half of the pair.
            const CAPRAHORN_B: u32 = 102_801;

            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DREAMBLOOM_SCENE,
            });
            m.apply(&hp(10, 60, 60, CAPRAHORN_A, 0));
            m.apply(&hp(11, 50, 50, CAPRAHORN_B, 0));
            m.apply(&hit(10, 1_000, false));
            m.apply(&hit(11, 1_100, false));

            // The first twin falls; its partner was hit moments ago and is
            // still very much being fought.
            m.apply(&hit(10, 2_000, true));

            assert_eq!(
                m.fight_end_ms, None,
                "the twin is still up and recently engaged"
            );
            assert_eq!(m.fight_state(2_100), FightState::Active);
        }
    }

    /// Issue #215: `SyncNearEntities.disappear` — an entity leaving the
    /// client's area of interest. The wire gives no reason for it, so the
    /// meter may only read a despawn as a death under the narrow rule
    /// documented on `Meter::apply_enemy_gone`; every other despawn must
    /// leave the encounter exactly as it found it.
    mod enemy_despawn {
        use super::*;

        /// Scene 13023, "Purge! Field of Forgotten Illusions" — the raid
        /// issue #210 was reported from, whose boss death never produced
        /// either of the two ordinary death signals.
        const RAID_SCENE: u32 = 13_023;
        /// Paradox-Calamity Remnant - Origin: the raid's first selection.
        const ORIGIN: u32 = 103_108;
        /// Paradox-Calamity Remnant - Continuation: the second selection.
        const CONTINUATION: u32 = 103_208;
        /// "Golden Nappo": named but `MonsterType == 0`, i.e. a trash add
        /// that `tables::is_boss_monster` rejects.
        const TRASH: u32 = 10_900;
        const BOSS_UID: i64 = 10;
        const OTHER_UID: i64 = 11;

        fn idle() -> u64 {
            FightConfig::default().idle_timeout_ms
        }

        fn in_raid() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: RAID_SCENE,
            });
            m
        }

        /// An ordinary dungeon: a `tables::is_dungeon_scene` id that is
        /// *not* in `phase::BOSS_SELECT_SCENES`, so issue #139 §8's
        /// objective gate still applies to it (issue #256).
        const DUNGEON_SCENE: u32 = 1_001;

        fn in_dungeon() -> Meter {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_SCENE,
            });
            m
        }

        /// The instance reporting a run in progress with one objective open
        /// and unfinished — the state issue #139 §8's gate reads.
        fn objective_running(m: &mut Meter) {
            m.apply(&ProtocolEvent::DungeonState {
                state: EDungeonState::Active,
                scene_uuid: None,
            });
            m.apply(&ProtocolEvent::DungeonObjective {
                target_id: 100,
                nums: Some(0),
                complete: Some(false),
            });
        }

        fn hp(uid: i64, curr: u64, max: u64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// An identity-only sync: the entity is recognized, but its health
        /// was never observed at all.
        fn hp_unknown(uid: i64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: None,
                max_hp: None,
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        fn hit(uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 500,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// A despawn carrying no tag 2 at all — the fallback path, and what
        /// every pre-#276 test in this module means by "gone".
        fn gone(uid: i64) -> ProtocolEvent {
            ProtocolEvent::EnemyGone {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                reason: None,
            }
        }

        /// A despawn carrying the server's own reason (issue #276).
        fn gone_because(uid: i64, reason: DisappearReason) -> ProtocolEvent {
            ProtocolEvent::EnemyGone {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                reason: Some(reason),
            }
        }

        /// The raid boss burned to 3% of its bar and hit a moment ago —
        /// every condition of the despawn rule but the despawn itself.
        fn pull() -> Meter {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 30_000, 1_000_000, ORIGIN, 1_500));
            m
        }

        #[test]
        fn a_damaged_low_hp_engaged_boss_despawning_ends_the_fight() {
            // Issue #210's shape: neither death signal ever arrives, and
            // the corpse is simply removed from AOI. That despawn is the
            // only evidence the meter will ever get.
            let mut m = pull();
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)));

            let reason = m.apply(&gone(BOSS_UID));

            assert_eq!(reason, None, "a despawn never reports a reset itself");
            assert_eq!(
                m.fight_end_ms,
                Some(1_000),
                "stamped at the last real damage, not at the despawn packet"
            );
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));
            assert_eq!(m.fight_state(2_000), FightState::Ended);
            assert!(
                !m.enemies[&ek(BOSS_UID)].is_alive(),
                "the boss must read as dead so nothing holds the pull open"
            );
        }

        #[test]
        fn a_boss_despawning_at_full_health_is_a_range_out_not_a_death() {
            // The case that makes the rule worth having: a boss the party
            // damaged and then ran away from evicts from AOI at full
            // health. Ending the fight here is strictly worse than doing
            // nothing.
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.fight_state(1_100), FightState::Active);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn a_boss_despawning_just_above_the_low_hp_threshold_is_not_a_death() {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            let just_above = (DESPAWN_DEATH_MAX_HP_PCT / 100.0 * 1_000_000.0) as u64 + 1;
            m.apply(&hp(BOSS_UID, just_above, 1_000_000, ORIGIN, 1_500));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn a_boss_despawning_exactly_at_the_low_hp_threshold_is_a_death() {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            let at_threshold = (DESPAWN_DEATH_MAX_HP_PCT / 100.0 * 1_000_000.0) as u64;
            m.apply(&hp(BOSS_UID, at_threshold, 1_000_000, ORIGIN, 1_500));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, Some(1_000));
        }

        #[test]
        fn a_boss_whose_health_was_never_observed_despawning_is_not_a_death() {
            // `EnemyState::is_alive` reads a never-observed enemy as alive,
            // deliberately. A despawn must not be the thing that converts
            // "no idea" into "dead": with no HP to check, the despawn is
            // just as likely a streaming eviction.
            let mut m = in_raid();
            m.apply(&hp_unknown(BOSS_UID, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn an_undamaged_boss_despawning_is_not_a_death() {
            // A boss standing in the room the party walked past, streamed
            // out again as they walk on. Nobody ever engaged it.
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(OTHER_UID, 1_000));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn an_unrecognized_monster_despawning_is_not_a_death() {
            // The same gate every other end-of-fight signal already has:
            // without it the biggest trash mob in the pull ends the fight
            // every time the AOI streams it out.
            let mut m = in_raid();
            m.apply(&hp(OTHER_UID, 100, 100_000, TRASH, 0));
            m.apply(&hit(OTHER_UID, 1_000));

            m.apply(&gone(OTHER_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(OTHER_UID)].is_alive());
        }

        #[test]
        fn a_boss_that_is_not_the_tracked_one_despawning_is_not_a_death() {
            // Only the boss the header is actually following can end the
            // fight this way. A second selection poked once and left at low
            // health despawning says nothing about the pull in progress.
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hit(OTHER_UID, 1_100));
            m.apply(&hp(OTHER_UID, 10_000, 1_000_000, CONTINUATION, 1_200));
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)), "Origin, the larger pool");

            m.apply(&gone(OTHER_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(OTHER_UID)].is_alive());
            assert_eq!(m.fight_state(1_300), FightState::Active);
        }

        #[test]
        fn a_boss_abandoned_long_ago_despawning_is_not_a_death() {
            // The party burned the boss low, gave up, and spent the next
            // few minutes on trash. The boss finally streams out of AOI.
            // `BOSS_ENGAGEMENT_WINDOW_MS` is what separates that from a
            // corpse being removed mid-pull, exactly as it does for
            // `engaged_boss_still_up` and `other_living_boss`.
            let mut m = pull();
            m.apply(&hp(OTHER_UID, 50_000, 50_000, TRASH, 1_600));
            let step = idle() - 1_000;
            let mut ts = 1_600;
            while ts <= 1_000 + BOSS_ENGAGEMENT_WINDOW_MS {
                ts += step;
                m.apply(&hit(OTHER_UID, ts));
            }
            assert_eq!(
                m.boss_entity,
                Some(ek(BOSS_UID)),
                "the recognized boss is tracked"
            );

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn a_corpse_despawning_after_an_ordinary_death_changes_nothing() {
            // The common case once this is live: the boss dies normally and
            // its corpse is removed a few seconds later. The despawn must
            // be inert — no second death rank, no re-latched end time.
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: BOSS_UID,
                target_kind: EntityKind::Monster,
                value: 500,
                is_dead: true,
                timestamp_ms: 2_000,
                ..Default::default()
            }));
            assert_eq!(m.fight_end_ms, Some(2_000));
            let rank = m.enemies[&ek(BOSS_UID)].death_order;
            let seen = m.deaths_seen;

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, Some(2_000));
            assert_eq!(m.enemies[&ek(BOSS_UID)].death_order, rank);
            assert_eq!(m.deaths_seen, seen, "the corpse must not die twice");
        }

        #[test]
        fn a_despawn_of_an_entity_the_meter_never_saw_is_inert() {
            let mut m = pull();
            let before = m.enemies.len();

            m.apply(&gone(9_999));

            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.enemies.len(), before, "no phantom enemy row");
        }

        #[test]
        fn the_end_on_boss_death_switch_also_governs_the_despawn_death() {
            let mut m = pull();
            m.set_fight_config(FightConfig {
                end_on_boss_death: false,
                ..FightConfig::default()
            });

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// PR #239 review, finding 2: the despawn death is routed through
        /// `end_fight_on_boss_death`, so `other_living_boss` still
        /// governs it — Dreambloom Ruins' Caprahorn pair is fought
        /// concurrently in a boss-select scene, and one twin's corpse
        /// vanishing must not freeze the meter while the other is still
        /// being hit. The damage-death path's equivalent is
        /// `multi_phase_boss::an_earlier_phase_dying_does_not_end_the_fight_while_a_later_one_lives`.
        #[test]
        fn a_despawn_does_not_end_the_fight_while_a_co_engaged_boss_lives() {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hit(OTHER_UID, 1_100));
            m.apply(&hp(BOSS_UID, 30_000, 2_000_000, ORIGIN, 1_200));
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)), "Origin, the larger pool");

            m.apply(&gone(BOSS_UID));

            assert!(
                !m.enemies[&ek(BOSS_UID)].is_alive(),
                "the despawn itself was read as a death -- this test is about \
                 the guard downstream of that, not about refusing the despawn"
            );
            assert!(m.enemies[&ek(OTHER_UID)].is_alive());
            assert_eq!(m.fight_end_ms, None, "the twin is still being fought");
            assert_eq!(m.fight_state(1_200), FightState::Active);
        }

        /// PR #239 review, finding 2, the other guard
        /// `end_fight_on_boss_death` keeps (issue #139 section 8): while the
        /// instance's own tracking says the run is going and the current
        /// objective is incomplete, this boss was a phase of it. The
        /// damage-death path's equivalent is
        /// `dungeon::a_boss_death_does_not_end_the_fight_while_the_objective_is_still_incomplete`.
        ///
        /// In an *ordinary* dungeon since issue #256: the gate no longer
        /// applies inside a boss-select raid, where the objective tracks the
        /// whole raid rather than the pull. See
        /// `a_raid_selections_death_ends_the_fight_despite_the_raids_own_objective`
        /// for that half.
        #[test]
        fn a_despawn_does_not_end_the_fight_while_the_objective_is_still_running() {
            let mut m = in_dungeon();
            objective_running(&mut m);
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 30_000, 1_000_000, ORIGIN, 1_500));

            m.apply(&gone(BOSS_UID));

            assert!(
                !m.enemies[&ek(BOSS_UID)].is_alive(),
                "the despawn itself was read as a death -- this test is about \
                 the guard downstream of that, not about refusing the despawn"
            );
            assert_eq!(
                m.fight_end_ms, None,
                "the instance's own objective says the run is still going"
            );
            assert_eq!(m.fight_state(1_600), FightState::Active);
        }

        /// The control for both guards above: same despawn, but the
        /// objective is complete and nothing else is up, so the ordinary
        /// despawn-death end still fires. Without this, the two tests above
        /// would pass just as happily if the despawn rule stopped working.
        #[test]
        fn a_despawn_ends_the_fight_once_the_objective_completes() {
            let mut m = in_dungeon();
            objective_running(&mut m);
            m.apply(&ProtocolEvent::DungeonObjective {
                target_id: 100,
                nums: None,
                complete: Some(true),
            });
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 30_000, 1_000_000, ORIGIN, 1_500));

            m.apply(&gone(BOSS_UID));

            assert_eq!(m.fight_end_ms, Some(1_000));
            assert_eq!(m.fight_state(1_600), FightState::Ended);
        }

        // -- The server's own reason, `DisappearEntity` tag 2 (issue #276) --
        //
        // `Dead` replaces the health threshold outright; every other stated
        // reason refuses the despawn *even when* the threshold would have
        // been satisfied; and a despawn with no tag 2 keeps issue #215's
        // heuristic exactly as it was.

        /// The whole point of issue #276: the server said the boss died, so
        /// no health inference is needed — and here there is none available,
        /// the boss was last seen at a full bar.
        #[test]
        fn a_dead_reason_ends_the_fight_without_the_hp_threshold() {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)));

            let reason = m.apply(&gone_because(BOSS_UID, DisappearReason::Dead));

            assert_eq!(reason, None, "a despawn never reports a reset itself");
            assert_eq!(
                m.fight_end_ms,
                Some(1_000),
                "stamped at the last real damage, exactly as the #215 path is"
            );
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));
            assert_eq!(m.fight_state(2_000), FightState::Ended);
            assert!(!m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// `Dead` replaces clause 8, not clause 3. An enemy whose health was
        /// never observed at all is unrankable (`rank_boss` drops a
        /// `(None, None)` HP pair outright), so it is never `boss_entity` and
        /// the despawn never reaches the reason check — the server saying it
        /// died does not change that. Pinned so a later widening of the rule
        /// has to be done deliberately.
        #[test]
        fn a_dead_reason_on_a_never_ranked_boss_is_still_not_a_death() {
            let mut m = in_raid();
            m.apply(&hp_unknown(BOSS_UID, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            assert_ne!(
                m.boss_entity,
                Some(ek(BOSS_UID)),
                "no HP ever synced: unrankable"
            );

            m.apply(&gone_because(BOSS_UID, DisappearReason::Dead));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// **The regression that matters most** (issue #243's false-positive
        /// class): `Destroy` is the mass-eviction reason, and the boss it
        /// evicts may well have been burned low. The health threshold is
        /// satisfied here — `pull()` leaves the boss at 3% — and the fight
        /// must still not end.
        #[test]
        fn a_destroy_reason_does_not_end_the_fight_even_at_low_health() {
            let mut m = pull();
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)));

            m.apply(&gone_because(BOSS_UID, DisappearReason::Destroy));

            assert_eq!(
                m.fight_end_ms, None,
                "the server said eviction, which outranks our HP inference"
            );
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
            assert_eq!(m.fight_state(2_000), FightState::Active);
        }

        /// Same shape as `Destroy`, for the zone-out reason — observed on
        /// characters only in our captures, but nothing on the wire promises
        /// a monster can never carry it.
        #[test]
        fn a_transfer_leave_reason_does_not_end_the_fight_even_at_low_health() {
            let mut m = pull();

            m.apply(&gone_because(BOSS_UID, DisappearReason::TransferLeave));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        #[test]
        fn a_transfer_pass_line_leave_reason_does_not_end_the_fight_even_at_low_health() {
            let mut m = pull();

            m.apply(&gone_because(
                BOSS_UID,
                DisappearReason::TransferPassLineLeave,
            ));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// An explicit `Normal` is ordinary streaming churn, and is *not*
        /// the same as no tag 2 at all: the server stated a reason, and the
        /// reason was "nothing happened".
        #[test]
        fn a_normal_reason_does_not_end_the_fight_even_at_low_health() {
            let mut m = pull();

            m.apply(&gone_because(BOSS_UID, DisappearReason::Normal));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// A future value nobody has decoded yet is refused rather than
        /// guessed at, in the same conservative direction as everything else
        /// in this rule.
        #[test]
        fn an_unrecognized_reason_does_not_end_the_fight_even_at_low_health() {
            let mut m = pull();

            m.apply(&gone_because(BOSS_UID, DisappearReason::Unknown(99)));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// `Dead` replaces clause 8 only. The identity clauses still stand:
        /// a trash mob the game says died cannot end a boss fight.
        #[test]
        fn a_dead_reason_on_an_unrecognized_monster_is_still_not_a_death() {
            let mut m = in_raid();
            m.apply(&hp(OTHER_UID, 100, 100_000, TRASH, 0));
            m.apply(&hit(OTHER_UID, 1_000));

            m.apply(&gone_because(OTHER_UID, DisappearReason::Dead));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(OTHER_UID)].is_alive());
        }

        /// Likewise for a boss nobody engaged: the party walked past it and
        /// something else killed it.
        #[test]
        fn a_dead_reason_on_an_undamaged_boss_is_still_not_a_death() {
            let mut m = in_raid();
            m.apply(&hp(BOSS_UID, 1_000, 1_000_000, ORIGIN, 0));
            m.apply(&hit(OTHER_UID, 1_000));

            m.apply(&gone_because(BOSS_UID, DisappearReason::Dead));

            assert_eq!(m.fight_end_ms, None);
            assert!(m.enemies[&ek(BOSS_UID)].is_alive());
        }

        /// The retained fallback, stated once explicitly rather than only
        /// implied by every `gone()` test above it: with no tag 2 on the
        /// packet the decision is still issue #215's health heuristic, in
        /// both directions.
        #[test]
        fn a_despawn_with_no_reason_still_uses_the_hp_fallback() {
            let mut low = pull();
            low.apply(&gone(BOSS_UID));
            assert_eq!(
                low.fight_end_ms,
                Some(1_000),
                "3% of the bar, no reason given: the heuristic still fires"
            );

            let mut full = in_raid();
            full.apply(&hp(BOSS_UID, 1_000_000, 1_000_000, ORIGIN, 0));
            full.apply(&hit(BOSS_UID, 1_000));
            full.apply(&gone(BOSS_UID));
            assert_eq!(
                full.fight_end_ms, None,
                "full bar, no reason given: still a range-out"
            );
        }

        /// Issue #256, the whole of it: scene 13023's despawn-as-death rule
        /// fired on every logged clear and the fight *still* ended
        /// `cause=idle_timeout` in the same second, three times out of three
        /// — six days of logs hold no `cause=boss_death` for any 103xxx boss
        /// at all. Both of `end_fight_on_boss_death`'s guards are reproduced
        /// here at once, in the shape the raid actually presents them:
        ///
        /// * the raid's *next* selection is standing there recognized and
        ///   alive but never engaged, which is `other_living_boss`'s case
        ///   (it must not count — issue #210/#211); and
        /// * the instance is streaming its own objective, which stays
        ///   incomplete until the last of the three Remnants falls, which
        ///   is issue #139 §8's gate — the one that actually refused, since
        ///   `other_living_boss` had already been taught to let a sequential
        ///   selection through.
        ///
        /// A boss-select raid's objective describes the raid, not the pull,
        /// so it must not gate a selection's death. `Some(1_000)` rather
        /// than any later value is the other half of the fix being real:
        /// the end is stamped at the last hit, not a minute later when the
        /// idle timeout would otherwise have reclaimed it.
        #[test]
        fn a_raid_selections_death_ends_the_fight_despite_the_raids_own_objective() {
            let mut m = in_raid();
            objective_running(&mut m);
            // The next selection: recognized, alive, and never touched.
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 60_000, 2_000_000, ORIGIN, 1_500));
            assert_eq!(m.boss_entity, Some(ek(BOSS_UID)), "the engaged selection");

            m.apply(&gone(BOSS_UID));

            assert_eq!(
                m.fight_end_ms,
                Some(1_000),
                "the selection's death ends the fight at the last hit"
            );
            assert_eq!(
                m.fight_end_boss_id,
                Some(ORIGIN),
                "a boss-death end -- only `end_fight_on_boss_death` sets this, \
                 so an idle-timeout end would leave it `None`"
            );
            assert_eq!(m.fight_state(1_600), FightState::Ended);
            assert!(
                m.enemies[&ek(OTHER_UID)].is_alive(),
                "the untouched next selection is still standing -- it simply \
                 has no say in whether this pull ended"
            );
        }

        /// Issue #295's real-world repro: a raid selection's death freezes
        /// the meter exactly as the test above shows, the party leaves the
        /// instance, and -- a long real-world gap later (the capture that
        /// reported #295 shows 43 minutes) -- queues into an entirely
        /// unrelated dungeon. That dungeon's own `Playing` signal is the
        /// authoritative "a fresh encounter is starting" event
        /// (`ResetReason::DungeonStarted`'s doc comment) and must clear the
        /// held fight the same way it does for any other dungeon entry, no
        /// matter how long the meter sat idle in between.
        #[test]
        fn a_new_dungeons_playing_signal_resets_a_fight_held_since_a_raid_selection_died() {
            let mut m = in_raid();
            objective_running(&mut m);
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 60_000, 2_000_000, ORIGIN, 1_500));
            m.apply(&gone(BOSS_UID));
            assert_eq!(
                m.fight_state(1_600),
                FightState::Ended,
                "sanity check: the kill froze the meter"
            );
            assert_eq!(
                m.snapshot(1_600).total_damage,
                500,
                "the held numbers from the kill"
            );

            // The party leaves the raid for the open world -- a reconnect
            // followed by the destination `Scene`, the same shape every
            // real zone transition takes in capture (issue #191).
            const TOWN_SCENE: u32 = 8;
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 60_000,
            });
            m.apply(&ProtocolEvent::Scene {
                level_map_id: TOWN_SCENE,
            });
            assert_eq!(
                m.fight_state(60_000),
                FightState::Ended,
                "issue #152: the kill's numbers stay on screen out in town"
            );

            // A long real-world gap, then a queue into a *different*
            // dungeon: another reconnect, its `Scene`, and the instance's
            // own `Playing`.
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 2_640_000,
            });
            let scene_reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_SCENE,
            });
            assert_eq!(
                scene_reason,
                Some(ResetReason::SceneChanged),
                "the confirmed different scene already fires the fast \
                 reset on its own -- this test's point is that the \
                 dungeon's `Playing` signal below still finishes the job \
                 (issue #295) rather than being needed to fire it"
            );
            let reason = m.apply(&ProtocolEvent::DungeonState {
                state: EDungeonState::Playing,
                scene_uuid: None,
            });

            assert_eq!(
                reason,
                Some(ResetReason::DungeonStarted),
                "issue #295: the new dungeon's own start signal must clear \
                 a fight held since a raid selection's death, even after \
                 the `Scene` event ahead of it already reset once"
            );
            assert_eq!(m.snapshot(2_640_100).total_damage, 0);
            assert_eq!(m.fight_state(2_640_100), FightState::Idle);
        }

        /// Issue #295's actual root cause: `ServerChanged` nulls `scene_id`
        /// before the `Scene` event that reports the destination lands --
        /// and every real zone transition in capture carries a
        /// `ServerChanged` first, a reconnect always accompanying a scene
        /// change. The `Scene` arm's `entering_dungeon` gate used to read
        /// `self.scene_id.is_some()` to tell "a specific previous scene is
        /// known" from "unknown", so in every real capture it saw `None`
        /// and never fired the fast `SceneChanged` reset -- leaving a fight
        /// held since a raid selection's death to wait on that new
        /// dungeon's own `Playing` signal (which can be minutes away, or
        /// require a manual reset) instead of clearing the moment the
        /// party is confirmed to be somewhere new.
        #[test]
        fn a_new_dungeon_scene_resets_a_fight_held_since_a_raid_selection_died_before_the_dungeon_even_starts()
         {
            let mut m = in_raid();
            objective_running(&mut m);
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hp(BOSS_UID, 60_000, 2_000_000, ORIGIN, 1_500));
            m.apply(&gone(BOSS_UID));
            assert_eq!(
                m.fight_state(1_600),
                FightState::Ended,
                "sanity check: the kill froze the meter"
            );

            // The party leaves the raid for the open world -- a reconnect
            // followed by the destination `Scene`, exactly as every real
            // zone transition in capture (issue #191).
            const TOWN_SCENE: u32 = 8;
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 60_000,
            });
            m.apply(&ProtocolEvent::Scene {
                level_map_id: TOWN_SCENE,
            });

            // Sometime later, another reconnect into a genuinely different
            // dungeon. The `Scene` event alone -- before that dungeon's own
            // `Playing` ever arrives -- must be enough to know this is not
            // the raid just left.
            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 2_640_000,
            });
            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_SCENE,
            });

            assert_eq!(
                reason,
                Some(ResetReason::SceneChanged),
                "issue #295: stepping into a confirmed different dungeon must \
                 reset immediately, without waiting on that dungeon's own \
                 `Playing` signal"
            );
            assert_eq!(m.snapshot(2_640_100).total_damage, 0);
            assert_eq!(m.fight_state(2_640_100), FightState::Idle);
        }

        /// The counterweight to the test above: dropping the objective gate
        /// in a boss-select scene must not drop `other_living_boss` with it.
        /// Dreambloom Ruins is a boss-select scene too, and its Caprahorn
        /// selection spawns two bosses fought *concurrently* — with the
        /// instance's objective open exactly as above, the surviving twin
        /// still has to hold the pull open.
        #[test]
        fn a_co_engaged_twin_still_holds_the_pull_open_with_the_objective_running() {
            const DREAMBLOOM_SCENE: u32 = 13_011;
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DREAMBLOOM_SCENE,
            });
            objective_running(&mut m);
            m.apply(&hp(BOSS_UID, 2_000_000, 2_000_000, ORIGIN, 0));
            m.apply(&hp(OTHER_UID, 1_000_000, 1_000_000, CONTINUATION, 0));
            m.apply(&hit(BOSS_UID, 1_000));
            m.apply(&hit(OTHER_UID, 1_100));
            m.apply(&hp(BOSS_UID, 30_000, 2_000_000, ORIGIN, 1_200));

            m.apply(&gone(BOSS_UID));

            assert!(
                !m.enemies[&ek(BOSS_UID)].is_alive(),
                "the despawn itself was still read as a death"
            );
            assert_eq!(m.fight_end_ms, None, "the twin is still being fought");
            assert_eq!(m.fight_state(1_300), FightState::Active);
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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
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
            assert_eq!(
                m.boss_entity,
                Some(ek(10)),
                "the larger-max-hp phase is boss"
            );

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
            // Pins `other_living_boss` on its own, without help from the
            // ranking key: an enemy with neither `max_hp` nor `curr_hp` is
            // unrankable, so `recompute_boss` cannot move `boss_entity` off the
            // dying phase and the guard is the only thing standing between
            // this fight and an early end. It also pins the asymmetry
            // documented on `EnemyState::is_alive` — never-observed HP counts
            // as alive, so the fight falls back to the idle timeout, which is
            // always safe.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(11, EntityKind::Monster),
                uid: 11,
                curr_hp: None,
                max_hp: None,
                monster_id: Some(CONTINUATION),
                timestamp_ms: 0,
            }));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 200, false));
            m.apply(&hit(10, 100, 300, true));

            assert_eq!(m.boss_entity, Some(ek(10)), "the other boss is unrankable");
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
        fn an_idle_timeout_end_on_a_phased_boss_arms_resumption() {
            // Issue #316: an idle-timeout end used to leave `fight_end_boss_id`
            // `None`, so phase resumption could never arm on that path at
            // all — and even fixing that alone would not have been enough,
            // since `engaged_boss_still_up` already burns the whole
            // `BOSS_ENGAGEMENT_WINDOW_MS` before a *recognized* boss's idle
            // timeout can even latch, and at stock config
            // `phase_resume_window_ms` is exactly as long, leaving no budget
            // for it once anchored on `fight_end_ms` (the last hit, not when
            // the end was actually observed). This test used to need
            // `phase_resume_window_ms` doubled just to land inside the
            // window at all — stock config throughout now.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));

            // `engaged_boss_still_up` releases the instant past
            // `BOSS_ENGAGEMENT_WINDOW_MS`; `tick` is what actually performs
            // the latch (`fight_state` alone only ever reads, never writes).
            let observed = 100 + BOSS_ENGAGEMENT_WINDOW_MS + 1;
            assert_eq!(
                m.tick(observed),
                FightState::Ended,
                "idle timeout latches once engagement lapses"
            );
            assert_eq!(
                m.fight_end_boss_id,
                Some(ORIGIN),
                "the engaged boss must be recorded so phase resume can arm"
            );

            m.apply(&hp(11, 500, 500, CONTINUATION, observed));
            let reason = m.apply(&hit(11, 700, observed + 100, false));

            assert_eq!(
                reason, None,
                "a phase change on an idle-timed-out boss resumes the held fight"
            );
            assert_eq!(m.fight_start_ms, Some(100), "the fight clock keeps running");
            assert_eq!(m.fight_end_ms, None);
            assert_eq!(m.fight_end_boss_id, None);
            assert_eq!(m.fight_state(observed + 200), FightState::Active);
            assert_eq!(
                m.snapshot(observed + 200).total_damage,
                1_200,
                "damage from before the phase change is still counted"
            );
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
            // `boss_entity == target_uid` path — no fall-through to the idle
            // timeout — and the header holds on the phase just killed rather
            // than snapping back to the larger-max-hp corpse.
            m.apply(&hit(12, 100, 11_000, true));
            assert_eq!(m.fight_end_ms, Some(11_000));
            assert_eq!(m.fight_state(11_100), FightState::Ended);
            assert_eq!(m.snapshot(11_100).encounter.boss_monster_id, Some(FINAL));
            assert_eq!(m.snapshot(11_100).total_damage, 600);

            // issue #150: this scene is not a curated raid scene, so nothing
            // asks the player to select a boss here.
            assert!(!m.snapshot(11_100).encounter.multi_boss_scene);
        }

        // -- Part B.5: `engaged_boss_monster_id` is deterministic ------------

        #[test]
        fn engaged_boss_monster_id_prefers_a_phased_candidate_regardless_of_map_order() {
            // Issue #316 review: `self.enemies` is a `HashMap`, and this
            // module's own doc comments (`other_living_boss`,
            // `engaged_boss_monster_id`) say a pull can have two bosses up
            // at once — so picking via `.values().find(..)` picked
            // whichever one the map happened to visit first, not a
            // meaningful answer. `ORIGIN` has a curated phase group
            // (`phase::has_phase_group`); `OTHER_BOSS` does not — only the
            // phased one is a sensible target to resume against, so it must
            // win no matter which uid the map visits first.
            for (first, second) in [(10i64, 11i64), (11, 10)] {
                let mut m = Meter::new();
                for &uid in &[first, second] {
                    let monster_id = if uid == 10 { ORIGIN } else { OTHER_BOSS };
                    m.apply(&hp(uid, 900, 1_000, monster_id, 0));
                    m.apply(&hit(uid, 100, 100, false));
                }
                assert_eq!(
                    m.engaged_boss_monster_id(),
                    Some(ORIGIN),
                    "insertion order {first},{second}: the phased boss must win"
                );
            }
        }

        #[test]
        fn engaged_boss_monster_id_breaks_an_equally_phased_tie_by_most_recent_damage() {
            // `ORIGIN` and `CONTINUATION` share a phase group, so the
            // phase-group signal ties between them and the tiebreak falls to
            // whichever was damaged more recently — the boss idling out
            // right now, not an earlier phase still sitting in the map
            // because its own death signal was never delivered.
            for (first, second) in [(10i64, 11i64), (11, 10)] {
                let mut m = Meter::new();
                for &uid in &[first, second] {
                    let (monster_id, ts) = if uid == 10 {
                        (ORIGIN, 100)
                    } else {
                        (CONTINUATION, 200)
                    };
                    m.apply(&hp(uid, 900, 1_000, monster_id, 0));
                    m.apply(&hit(uid, 100, ts, false));
                }
                assert_eq!(
                    m.engaged_boss_monster_id(),
                    Some(CONTINUATION),
                    "insertion order {first},{second}: the more recently damaged phase must win"
                );
            }
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
        fn a_straggling_add_inside_the_window_does_not_clear_an_idle_timeout_hold() {
            // Issue #316: now that an idle-timeout end on a phased boss arms
            // `fight_end_boss_id` too (see
            // `an_idle_timeout_end_on_a_phased_boss_arms_resumption`), it
            // must behave exactly like a boss-death hold for every other
            // purpose the arming exists for — including withholding an
            // unrelated add's hit during the transition window instead of
            // reading it as the next pull, the same contract
            // `a_straggling_add_inside_the_window_does_not_clear_the_held_fight`
            // pins for a boss-death end. Stock config throughout.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));

            let observed = 100 + BOSS_ENGAGEMENT_WINDOW_MS + 1;
            assert_eq!(m.tick(observed), FightState::Ended);
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            m.apply(&hp(12, 100, 100, TRASH, observed + 100));
            let reason = m.apply(&hit(12, 50, observed + 200, false));

            assert_eq!(reason, None, "an add is not the next pull");
            assert_eq!(m.fight_end_ms, Some(100), "the hold stays armed");
            assert_eq!(m.fight_end_boss_id, Some(ORIGIN));

            // ...and the real next phase still resumes into the same fight.
            m.apply(&hp(11, 500, 500, CONTINUATION, observed + 300));
            m.apply(&hit(11, 700, observed + 400, false));

            assert_eq!(m.fight_start_ms, Some(100));
            assert_eq!(
                m.snapshot(observed + 500).total_damage,
                1_200,
                "the add's hit was held, not counted"
            );
        }

        #[test]
        fn a_server_changed_reconnect_after_a_phased_bosss_death_does_not_withhold_the_next_fight()
        {
            // Issue #316: `fight_end_boss_id` used to survive a
            // `ServerChanged` reconnect — only `reset` cleared it, and a
            // fight already held runs no reset on this path (see the
            // `ServerChanged` arm's own comment on issue #152). But
            // `enemies` *was* cleared there, so `target_monster_id` could
            // never resolve into the new instance, `withholds_new_fight`
            // read every hit's unresolved target as "undecided, could
            // still be the next phase" forever, and `apply_damage` silently
            // dropped the reconnecting player's damage — first hit
            // included — until `phase_resume_window_ms` finally lapsed on
            // its own.
            let mut m = Meter::new();
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            assert_eq!(
                m.fight_end_boss_id,
                Some(ORIGIN),
                "sanity check: the kill armed the hold"
            );

            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 2_000,
            });
            assert_eq!(
                m.fight_end_boss_id, None,
                "the reconnect must drop the stale arming"
            );
            assert_eq!(
                m.fight_state(2_000),
                FightState::Ended,
                "issue #152: the kill's numbers stay held across the reconnect"
            );

            // The reconnecting player's first hit, on a target the new
            // instance has not even named yet — exactly the packet-order
            // shape that used to read as "undecided" and get dropped.
            let reason = m.apply(&hit(20, 700, 2_100, false));

            assert_eq!(
                reason,
                Some(ResetReason::NewFight),
                "must start the next fight, not sit withheld forever"
            );
            assert_eq!(m.fight_start_ms, Some(2_100));
            assert_eq!(
                m.snapshot(2_200).total_damage,
                700,
                "the reconnecting hit must be counted, not dropped"
            );
        }

        #[test]
        fn a_scene_change_into_a_different_dungeon_after_a_phased_bosss_death_does_not_withhold_the_next_fight()
         {
            // Issue #316 (`Scene` counterpart to the `ServerChanged`
            // regression above): the `Scene` arm's `entering_dungeon`
            // branch clears `fight_end_boss_id` for the same reason the
            // `ServerChanged` arm does — it empties `enemies`, and a stale
            // arming against that now-empty map would read the next
            // dungeon's first hit as "undecided, could still be the next
            // phase" and withhold it (and every one after it) until
            // `phase_resume_window_ms` lapsed on its own.
            const DUNGEON_A: u32 = 1_001;
            const DUNGEON_B: u32 = 40_001;

            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_A,
            });
            m.apply(&hp(10, 900, 1_000, ORIGIN, 0));
            m.apply(&hit(10, 500, 100, false));
            m.apply(&hit(10, 500, 1_000, true));
            assert_eq!(
                m.fight_end_boss_id,
                Some(ORIGIN),
                "sanity check: the kill armed the hold"
            );

            let reason = m.apply(&ProtocolEvent::Scene {
                level_map_id: DUNGEON_B,
            });
            assert_eq!(
                reason,
                Some(ResetReason::SceneChanged),
                "sanity check: this is the dungeon-to-dungeon transition"
            );
            assert_eq!(
                m.fight_end_boss_id, None,
                "the dungeon-to-dungeon transition must drop the stale arming"
            );

            // The new dungeon's first hit, on a target it has not even
            // named yet — exactly the packet-order shape that used to read
            // as "undecided" and get dropped. Unlike the `ServerChanged`
            // case, the `Scene` arm's own `SceneChanged` reset above already
            // ran (it does not defer to a held-fight `NewFight` reset the
            // way `ServerChanged` does), so this lands as an ordinary fresh
            // hit rather than one that itself triggers a reset — the
            // regression this pins is that it is counted at all, not
            // silently withheld forever behind a stale `fight_end_boss_id`.
            let reason = m.apply(&hit(20, 700, 2_100, false));

            assert_eq!(
                reason, None,
                "no reset fires here — the Scene arm's own reset already ran"
            );
            assert_eq!(m.fight_start_ms, Some(2_100));
            assert_eq!(
                m.snapshot(2_200).total_damage,
                700,
                "the new dungeon's hit must be counted, not dropped"
            );
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
            assert_eq!(m.enemies[&ek(10)].curr_hp, Some(900), "no sync ever hit 0");

            m.reset(ResetReason::Manual, 300);

            // Next pull. A straggler DoT tick puts the corpse back into
            // `recompute_boss`'s pool alongside the boss actually being
            // fought.
            m.apply(&hit(10, 10, 400, false));
            m.apply(&hp(11, 500, 500, OTHER_BOSS, 400));
            m.apply(&hit(11, 100, 500, false));

            assert!(!m.enemies[&ek(10)].is_alive(), "the corpse is still dead");
            assert_eq!(
                m.boss_entity,
                Some(ek(11)),
                "the living boss keeps the header"
            );

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

            assert_eq!(m.enemies[&ek(10)].death_order, None, "the rank is cleared");
            assert!(m.enemies[&ek(10)].is_alive());

            m.apply(&hit(10, 100, 500, false));
            m.apply(&hit(10, 100, 600, true));
            assert_eq!(m.fight_end_ms, Some(600), "the re-pull ends on its kill");
        }

        #[test]
        fn a_corpse_resyncing_upward_mid_fight_stays_dead() {
            // The `took_damage` gate on that respawn signal. Inside a fight a
            // dead phase's HP resyncing above zero is an artefact, and the
            // death latch must survive it — otherwise the corpse re-enters
            // `other_living_boss` and blocks the living phase's own end.
            let mut m = Meter::new();
            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            m.apply(&hit(10, 100, 200, true));
            assert_eq!(m.fight_end_ms, None, "the other phase is still up");

            m.apply(&hp(10, 1_500, 2_000, ORIGIN, 250));
            assert!(!m.enemies[&ek(10)].is_alive(), "a resync is not a respawn");

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

            assert_eq!(m.boss_entity, Some(ek(10)));
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

            assert_eq!(m.boss_entity, Some(ek(11)));
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

            assert_eq!(m.boss_entity, Some(ek(10)));
            assert_eq!(m.snapshot(300).encounter.boss_monster_id, Some(ORIGIN));
        }

        #[test]
        fn selection_moves_to_the_living_phase_even_with_a_smaller_hp_pool() {
            let mut m = Meter::new();
            m.apply(&hp(10, 2_000, 2_000, ORIGIN, 0));
            m.apply(&hp(11, 500, 500, CONTINUATION, 0));
            m.apply(&hit(10, 100, 100, false));
            m.apply(&hit(11, 100, 150, false));
            assert_eq!(
                m.boss_entity,
                Some(ek(10)),
                "both alive: the larger pool wins"
            );

            m.apply(&hit(10, 100, 200, true));
            assert_eq!(m.boss_entity, Some(ek(11)), "the living phase takes over");
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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
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
            assert_eq!(snap.encounter.boss_name, Some("Ignisor"));
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
                entity: EntityId::from_display_uid(10, EntityKind::Monster),
                uid: 10,
                curr_hp: Some(5_000_000),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: 0,
            }));
            let snap = m.snapshot(1000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert_eq!(snap.encounter.boss_name, Some("Ignisor"));
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
                entity: EntityId::from_display_uid(11, EntityKind::Monster),
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
                entity: EntityId::from_display_uid(10, EntityKind::Monster),
                uid: 10,
                curr_hp: Some(2_000_000),
                max_hp: None,
                monster_id: Some(103),
                timestamp_ms: 0,
            }));
            // Untouched trash add with a bigger raw number, same tier.
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(11, EntityKind::Monster),
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
                entity: EntityId::from_display_uid(10, EntityKind::Monster),
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
                entity: EntityId::from_display_uid(10, EntityKind::Monster),
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

        // Issue #201: the curated `tables::SCENE_FINAL_BOSSES` path that
        // replaced issue #125's runtime latch is covered by `mod
        // scene_final_boss` above.
    }

    /// issue #69: `scene_transition_log`/`boss_transition_log` are the pure
    /// decision functions behind `Meter::apply`'s and `recompute_boss`'s
    /// sparse diagnostics. Tested directly (rather than by capturing actual
    /// `log::info!` output, which this workspace has no harness for) so
    /// "logs on change, silent on repeat" is asserted without needing one.
    mod diagnostics {
        use super::*;
        use std::sync::{Mutex, Once};

        /// A recognized boss id used by no other test in this file, so a
        /// line found in the shared capture buffer below can only have come
        /// from the test that logged it.
        const DIAG_BOSS: u32 = 33_601;

        /// Every line logged since [`install_capture`] ran, from anywhere in
        /// this test binary: `log` allows one global logger per process, so
        /// the buffer is necessarily shared. Assertions on it must therefore
        /// be *positive* ("this exact line was logged") — an absence says
        /// nothing, and a presence is only attributable when the line is
        /// unique to one test, which is what `DIAG_BOSS` guarantees.
        static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

        struct CaptureLogger;

        impl log::Log for CaptureLogger {
            fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
                true
            }

            fn log(&self, record: &log::Record<'_>) {
                if let Ok(mut captured) = CAPTURED.lock() {
                    captured.push(record.args().to_string());
                }
            }

            fn flush(&self) {}
        }

        /// Installs [`CAPTURE_LOGGER`] once per process. Idempotent, so any
        /// number of tests can call it, in any order, from any thread.
        fn install_capture() {
            static INSTALL: Once = Once::new();
            INSTALL.call_once(|| {
                let _ = log::set_logger(&CAPTURE_LOGGER);
                log::set_max_level(log::LevelFilter::Info);
            });
        }

        /// Whether any captured line contains `needle`.
        fn logged(needle: &str) -> bool {
            CAPTURED
                .lock()
                .map(|captured| captured.iter().any(|line| line.contains(needle)))
                .unwrap_or(false)
        }

        #[test]
        fn a_server_change_logs_the_boss_the_fight_was_on() {
            // PR #163 review, finding 3: the `ServerChanged` arm cleared
            // `enemies` and `boss_entity` before latching the fight end, and
            // `latch_fight_end` reads the boss identity out of exactly
            // those — so this diagnostic always read
            // `boss_monster_id=<unknown>`, losing the one fact it exists to
            // record about a pull cut short by a reconnect.
            install_capture();

            let mut m = Meter::new();
            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(10, EntityKind::Monster),
                uid: 10,
                curr_hp: Some(500_000),
                max_hp: Some(1_000_000),
                monster_id: Some(DIAG_BOSS),
                timestamp_ms: 0,
            }));
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: 10,
                target_kind: EntityKind::Monster,
                value: 1_000,
                timestamp_ms: 1_000,
                ..Default::default()
            }));
            assert_eq!(m.boss_entity, Some(ek(10)));

            m.apply(&ProtocolEvent::ServerChanged {
                timestamp_ms: 2_000,
            });
            assert_eq!(m.fight_end_ms, Some(2_000), "the fight is frozen");

            assert!(
                logged(&format!("cause=server_changed boss_monster_id={DIAG_BOSS}")),
                "the fight-end line must still name the boss the fight was on"
            );
        }

        /// Issue #284: `reset`'s `party_down` used to be
        /// `players.values().filter(|p| p.deaths > 0).count()` — a
        /// cumulative "ever died" tally that a battle rez can never bring
        /// back down, so it stays true for the rest of the pull. This pins
        /// the fix: a player who died once and was rezzed (their next
        /// action clears `alive`, per `party_is_wiped`'s doc comment) must
        /// not still count as "down" in the reset diagnostic, the same way
        /// `party_mostly_down`/`party_is_wiped` already read `alive` rather
        /// than `deaths` for the wipe-vs-rollback decision itself.
        #[test]
        fn reset_log_reports_players_currently_down_not_a_cumulative_death_count() {
            install_capture();

            let mut m = Meter::new();
            // uid 1 dies to a monster...
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 90,
                attacker_kind: EntityKind::Monster,
                target_uid: 1,
                target_kind: EntityKind::Player,
                value: 500,
                is_dead: true,
                timestamp_ms: 1_000,
                ..Default::default()
            }));
            // ...uid 2 is just a second known party member, never down...
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 2,
                attacker_kind: EntityKind::Player,
                target_uid: 90,
                target_kind: EntityKind::Monster,
                value: 1_000,
                timestamp_ms: 1_000,
                ..Default::default()
            }));
            // ...and uid 1 is battle-rezzed: their next action (a hit)
            // clears `alive` back to true, but `deaths` stays 1 forever.
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: 90,
                target_kind: EntityKind::Monster,
                value: 1_000,
                timestamp_ms: 2_000,
                ..Default::default()
            }));
            assert_eq!(
                m.players.get(&pk(1)).map(|p| (p.deaths, p.alive)),
                Some((1, true)),
                "the rez must leave a cumulative death on the board but the player standing"
            );

            m.reset(ResetReason::Manual, 3_000);

            assert!(
                logged("party_down=0/2"),
                "nobody is down right now, so the reset diagnostic must not \
                 report the one player who merely died earlier in the pull"
            );
            // No negative assertion here (e.g. `!logged("party_down=1/2")`):
            // per this module's doc comment above, `CAPTURED` is shared by
            // every test in the binary and never cleared, so an *absence*
            // proves nothing and a generic line like `party_down=1/2` is
            // not unique to this test -- `reset_clears_deaths` (the
            // `deaths` module) legitimately logs that exact line for an
            // unrelated scenario, so asserting its absence here raced
            // against test scheduling instead of testing this fix. The
            // positive assertion above already pins the fix: if the
            // cumulative-count bug regressed, this reset would log
            // `party_down=1/2` instead of `0/2` and that assertion alone
            // would fail.
        }

        /// issue #256: pins every field of the new "boss death did not end
        /// the fight" diagnostic itself, so a typo swapping
        /// `other_living_boss` and `dungeon_objective_still_running` (or
        /// any other field) in the format string fails a test instead of
        /// only being caught by someone reading the log later.
        #[test]
        fn boss_death_that_does_not_end_the_fight_logs_every_guard_input() {
            install_capture();

            const DIAG_UID: i64 = 20;
            let mut m = Meter::new();

            // A dungeon whose own objective is still running holds the
            // gate open (issue #139 §8) even though no other living boss
            // exists — so this death falls through to the log rather than
            // ending the fight.
            m.apply(&ProtocolEvent::DungeonState {
                state: EDungeonState::Active,
                scene_uuid: None,
            });
            m.apply(&ProtocolEvent::DungeonObjective {
                target_id: 700,
                nums: Some(0),
                complete: Some(false),
            });

            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(DIAG_UID, EntityKind::Monster),
                uid: DIAG_UID,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(DIAG_BOSS),
                timestamp_ms: 0,
            }));
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: DIAG_UID,
                target_kind: EntityKind::Monster,
                value: 100,
                timestamp_ms: 1_000,
                ..Default::default()
            }));
            assert_eq!(m.boss_entity, Some(ek(DIAG_UID)));

            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 1,
                attacker_kind: EntityKind::Player,
                target_uid: DIAG_UID,
                target_kind: EntityKind::Monster,
                value: 100,
                is_dead: true,
                timestamp_ms: 2_000,
                ..Default::default()
            }));

            assert_eq!(
                m.fight_end_ms, None,
                "the still-running objective must hold the fight open"
            );
            assert!(
                logged(&format!(
                    "encounter: boss death of uid={DIAG_UID} monster_id={DIAG_BOSS} did not end the fight: \
                     other_living_boss=-1 dungeon_objective_still_running=true \
                     scene=-1 boss_select=false dungeon_state=Some(Active) current_objective=Some(700) \
                     objective_complete=Some(false)"
                )),
                "the diagnostic line must name every guard input, not just say a boss death was dropped"
            );
        }

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
            assert!(boss_transition_log(None, Some(ek(10)), Some(103)).is_some());
            assert!(boss_transition_log(Some(ek(10)), Some(ek(10)), Some(103)).is_none());
            assert!(boss_transition_log(Some(ek(10)), Some(ek(11)), Some(103)).is_some());
            // Boss target clearing (a real transition) still logs.
            assert!(boss_transition_log(Some(ek(10)), None, None).is_some());
            // No-op stays silent even when both sides are already empty.
            assert!(boss_transition_log(None, None, None).is_none());
        }

        #[test]
        fn monster_id_change_log_fires_only_when_the_id_actually_changes() {
            // Issue #313: the first id ever seen for a uid is not a change
            // — `boss_transition_log` already covers that moment — and a
            // resync repeating the same id is the #87 flood waiting to
            // happen.
            assert_eq!(monster_id_change_log(1, None, 20_004), None);
            assert_eq!(monster_id_change_log(1, Some(20_004), 20_004), None);

            // The reported rewrite: uid 1 goes from a recognized boss to an
            // id that was not one, with the uid — and so
            // `boss_transition_log` — never moving.
            let msg = monster_id_change_log(1, Some(20_004), 3_000_063).unwrap();
            assert!(msg.contains("uid=1"), "{msg}");
            assert!(msg.contains("20004"), "{msg}");
            assert!(msg.contains("3000063"), "{msg}");
            assert!(msg.contains("Ignisor"), "{msg}");
            assert!(msg.contains("Denvel"), "{msg}");
        }

        #[test]
        fn boss_transition_log_reports_recognition_and_the_resolved_name() {
            // Recognized boss id with a catalogued name.
            let msg = boss_transition_log(None, Some(ek(10)), Some(103)).unwrap();
            assert!(msg.contains("monster_id=103"));
            assert!(msg.contains("recognized_boss=true"));
            assert!(msg.contains("name=Ignisor"));

            // A monster id outside the boss table: recognized_boss=false,
            // name still resolved if catalogued (boss_monster_id itself is
            // real data regardless of recognition — see `EncounterInfo`'s
            // doc comment).
            let msg = boss_transition_log(None, Some(ek(10)), Some(10_900)).unwrap();
            assert!(msg.contains("recognized_boss=false"));

            // Unknown monster id entirely.
            let msg = boss_transition_log(None, Some(ek(10)), None).unwrap();
            assert!(msg.contains("uid=10"));
            assert!(msg.contains("monster_id=<unknown>"));

            // Boss target cleared.
            let msg = boss_transition_log(Some(ek(10)), None, None).unwrap();
            assert!(msg.contains("cleared"));
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
            assert!(msg.contains("name=Ignisor"));

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

    /// Issue #317: `apply_enemy_hp` treats an existing uid reporting a new
    /// `monster_id` as the uid being recycled onto a different entity, not
    /// one live entity being re-templated (`uid = uuid >> 16` puts uid=1 at
    /// the very first slot ever allocated, and every curated `phase.rs`
    /// group stays inside one id family) — so the rest of `EnemyState` is
    /// reset to a fresh entity's starting values on that transition.
    mod monster_id_reset {
        use super::*;

        /// "Ignisor" (103), a recognized boss.
        const OLD_BOSS: u32 = 103;
        /// "Golden Nappo" (10_900): not a `BOSS_MONSTER_IDS` entry, so the
        /// id crosses out of `OLD_BOSS`'s family the way 20004 -> 3000063
        /// did in the reported World Dominator log.
        const NEW_BOSS: u32 = 10_900;
        const UID: i64 = 1;

        fn hp(monster_id: u32, curr: u64, max: u64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(UID, EntityKind::Monster),
                uid: UID,
                curr_hp: Some(curr),
                max_hp: Some(max),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        fn boss_hit(uid: i64, ts: u64) -> ProtocolEvent {
            ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 99,
                attacker_kind: EntityKind::Player,
                target_uid: uid,
                target_kind: EntityKind::Monster,
                value: 1,
                timestamp_ms: ts,
                ..Default::default()
            })
        }

        /// (a) A boss burned to 40%, then the same uid reports a new
        /// `monster_id` with full HP: `pct()` must read the *new* entity's
        /// 100%, not 40% of the old one's pool, and every other HP-derived
        /// or fight-history field must read like a never-before-seen enemy.
        #[test]
        fn a_monster_id_change_resets_hp_derived_and_history_fields() {
            let mut m = Meter::new();
            m.apply(&hp(OLD_BOSS, 1_000_000, 1_000_000, 0));
            m.apply(&boss_hit(UID, 100));
            m.apply(&hp(OLD_BOSS, 400_000, 1_000_000, 200));

            let old = &m.enemies[&ek(UID)];
            assert_eq!(old.pct(), Some(40.0));
            assert_eq!(old.lowest_pct, Some(40.0));
            assert!(old.took_damage);

            m.apply(&hp(NEW_BOSS, 1_000_000, 1_000_000, 300));

            let new = &m.enemies[&ek(UID)];
            assert_eq!(new.monster_id, Some(NEW_BOSS));
            assert_eq!(new.pct(), Some(100.0));
            assert_eq!(
                new.lowest_pct,
                Some(100.0),
                "the old entity's dip must not survive as the new entity's floor"
            );
            assert_eq!(new.death_order, None);
            assert!(!new.took_damage);
            assert_eq!(new.last_damaged_ms, None);
        }

        /// (b) A dead entity's uid reused with a new `monster_id` reads
        /// alive again — the recycled uid is a different entity, not the
        /// same corpse resyncing.
        #[test]
        fn a_dead_uids_monster_id_change_comes_back_alive() {
            let mut m = Meter::new();
            m.apply(&hp(OLD_BOSS, 1_000_000, 1_000_000, 0));
            m.apply(&boss_hit(UID, 100));
            m.apply(&ProtocolEvent::Damage(DamageEvent {
                attacker_uid: 99,
                attacker_kind: EntityKind::Player,
                target_uid: UID,
                target_kind: EntityKind::Monster,
                value: 1,
                is_dead: true,
                timestamp_ms: 200,
                ..Default::default()
            }));
            assert!(!m.enemies[&ek(UID)].is_alive());

            m.apply(&hp(NEW_BOSS, 500_000, 1_000_000, 300));

            assert!(m.enemies[&ek(UID)].is_alive());
            assert_eq!(m.enemies[&ek(UID)].death_order, None);
        }

        /// (c) A resync repeating the *same* `monster_id` is not a change
        /// and must not disturb anything already accumulated.
        #[test]
        fn the_same_monster_id_repeated_resets_nothing() {
            let mut m = Meter::new();
            m.apply(&hp(OLD_BOSS, 1_000_000, 1_000_000, 0));
            m.apply(&boss_hit(UID, 100));
            m.apply(&hp(OLD_BOSS, 400_000, 1_000_000, 200));

            let before = m.enemies[&ek(UID)];
            m.apply(&hp(OLD_BOSS, 400_000, 1_000_000, 300));
            let after = m.enemies[&ek(UID)];

            assert_eq!(before.lowest_pct, after.lowest_pct);
            assert_eq!(before.took_damage, after.took_damage);
            assert_eq!(before.death_order, after.death_order);
            assert_eq!(before.last_damaged_ms, after.last_damaged_ms);
            assert_eq!(after.monster_id, Some(OLD_BOSS));
        }

        /// (d) An AOI-sync delta can carry `monster_id` alone, with no
        /// `curr_hp`/`max_hp` (see `enemy_hp_from_attrs`). The state still
        /// resets — the recycled uid is still a new entity — but with no HP
        /// fields in the packet to reapply, `curr_hp`/`max_hp` land as `None`
        /// and `pct()` reads `None` until the next HP-bearing packet for this
        /// uid arrives.
        #[test]
        fn monster_id_only_delta_resets_state_and_leaves_hp_unknown() {
            let mut m = Meter::new();
            m.apply(&hp(OLD_BOSS, 1_000_000, 1_000_000, 0));
            m.apply(&boss_hit(UID, 100));
            m.apply(&hp(OLD_BOSS, 400_000, 1_000_000, 200));
            assert_eq!(m.enemies[&ek(UID)].pct(), Some(40.0));

            m.apply(&ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(UID, EntityKind::Monster),
                uid: UID,
                curr_hp: None,
                max_hp: None,
                monster_id: Some(NEW_BOSS),
                timestamp_ms: 300,
            }));

            let reset = &m.enemies[&ek(UID)];
            assert_eq!(reset.monster_id, Some(NEW_BOSS));
            assert_eq!(reset.curr_hp, None);
            assert_eq!(reset.max_hp, None);
            assert_eq!(
                reset.pct(),
                None,
                "no HP in the delta means no HP for the new entity yet"
            );
            assert_eq!(reset.lowest_pct, None);
            assert!(!reset.took_damage);
            assert_eq!(reset.death_order, None);
            assert_eq!(reset.last_damaged_ms, None);

            // The next HP-bearing packet restores a reading.
            m.apply(&hp(NEW_BOSS, 500_000, 1_000_000, 400));
            assert_eq!(m.enemies[&ek(UID)].pct(), Some(50.0));
        }
    }

    /// Issue #152: while a finished fight is held on screen, the header has
    /// to keep naming *that* fight. Zoning out clears the live boss and
    /// scene (`ServerChanged`), so the identity is captured while the fight
    /// is live and released only when the hold is.
    mod held_fight_identity {
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
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// The boss's HP syncing to zero: the ordinary end of a pull, and
        /// the one that works inside a dungeon scene (issue #151 holds the
        /// idle timeout off for as long as an engaged boss is still up).
        fn killed(uid: i64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(0),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        /// Walks the reported zone-out: `ServerChanged` first, then the
        /// town's `Scene`.
        fn zone_out_to_town(m: &mut Meter, ts: u64) {
            m.apply(&ProtocolEvent::ServerChanged { timestamp_ms: ts });
            m.apply(&ProtocolEvent::Scene { level_map_id: 8 });
        }

        #[test]
        fn zoning_out_while_a_fight_is_held_keeps_the_header_on_that_fight() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 1_000));
            m.apply(&hp(10, 103, 1_000));
            zone_out_to_town(&mut m, 5_000);

            assert_eq!(m.fight_state(6_000), FightState::Ended);
            let snap = m.snapshot(6_000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert!(snap.encounter.is_boss);
            assert_eq!(snap.encounter.boss_name, Some("Ignisor"));
            assert_eq!(snap.encounter.scene_id, Some(1001));
            assert_eq!(snap.encounter.scene_name, Some("Tina's Mindrealm"));
        }

        #[test]
        fn the_header_follows_live_state_again_once_the_hold_is_released() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 1_000));
            m.apply(&hp(10, 103, 1_000));
            zone_out_to_town(&mut m, 5_000);
            assert_eq!(m.snapshot(6_000).encounter.scene_id, Some(1001));

            // The next fight's first hit ends the hold (`NewFight`), so the
            // header must snap back to where the player actually is.
            m.apply(&boss_hit(11, 60_000));
            m.apply(&hp(11, 1342, 60_000));
            let snap = m.snapshot(61_000);
            assert_eq!(snap.encounter.scene_id, Some(8));
            assert_eq!(snap.encounter.scene_name, Some("Asterleeds"));
            assert_eq!(snap.encounter.boss_name, None);
            assert!(!snap.encounter.is_boss);
        }

        #[test]
        fn a_held_trash_pull_pins_no_boss_and_leaves_the_scene_live() {
            // 1342 ("Boss - Battle Mech 03") is not a genuine boss, so there
            // is no fight identity worth holding: the header keeps its
            // pre-#152 behaviour and follows the live scene.
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 1_000));
            m.apply(&hp(10, 1342, 1_000));
            zone_out_to_town(&mut m, 5_000);

            let snap = m.snapshot(6_000);
            assert_eq!(snap.encounter.boss_monster_id, None);
            assert_eq!(snap.encounter.scene_id, Some(8));
        }

        #[test]
        fn a_scene_that_arrives_after_the_boss_still_captions_the_held_fight() {
            // `EnterScene` can land after the pull has already started (it
            // does in `replay_dump`'s real capture), and only damage/HP
            // events refresh the captured identity — so a held fight whose
            // scene was never captured must still show the scene the meter
            // does know, rather than blanking the subtitle.
            //
            // The pull is ended by the boss dying rather than by the idle
            // timeout: issue #151 holds a fight open for as long as an
            // engaged boss is still standing in a dungeon, so a live boss
            // and a long silence no longer add up to an ended fight. The
            // kill lands while `scene_id` is still unknown, which is what
            // keeps the captured identity's scene `None` — exactly the
            // state under test.
            let mut m = Meter::new();
            m.apply(&boss_hit(10, 1_000));
            m.apply(&hp(10, 103, 1_000));
            m.apply(&killed(10, 103, 2_000));
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });

            assert_eq!(m.fight_state(60_000), FightState::Ended);
            let snap = m.snapshot(60_000);
            assert_eq!(snap.encounter.boss_monster_id, Some(103));
            assert_eq!(snap.encounter.scene_id, Some(1001));
            assert_eq!(snap.encounter.scene_name, Some("Tina's Mindrealm"));
        }

        #[test]
        fn a_manual_reset_releases_the_held_identity() {
            let mut m = Meter::new();
            m.apply(&ProtocolEvent::Scene { level_map_id: 1001 });
            m.apply(&boss_hit(10, 1_000));
            m.apply(&hp(10, 103, 1_000));
            zone_out_to_town(&mut m, 5_000);
            m.reset(ResetReason::Manual, 6_000);

            let snap = m.snapshot(7_000);
            assert_eq!(snap.encounter.boss_monster_id, None);
            assert_eq!(snap.encounter.scene_id, Some(8));
        }
    }

    /// Issue #139 slice 2: `DungeonState`/`DungeonObjective`/`DungeonVar`
    /// driving `reset`/`latch_fight_end` directly, plus the raid-boss reset
    /// detector and the boss-death gate (spec "Meter behaviour" §§1-8).
    mod dungeon {
        use super::*;

        /// 103 = "Ignisor", a `tables::BOSS_MONSTER_IDS` entry, same id
        /// `held_fight_identity` above already relies on being recognized.
        const BOSS: u32 = 103;
        const BOSS_UID: i64 = 10;

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

        fn hp(uid: i64, monster_id: u32, ts: u64) -> ProtocolEvent {
            ProtocolEvent::EnemyHp(EnemyHp {
                entity: EntityId::from_display_uid(uid, EntityKind::Monster),
                uid,
                curr_hp: Some(100),
                max_hp: Some(100),
                monster_id: Some(monster_id),
                timestamp_ms: ts,
            })
        }

        fn dungeon_state(state: EDungeonState) -> ProtocolEvent {
            ProtocolEvent::DungeonState {
                state,
                scene_uuid: None,
            }
        }

        fn objective(target_id: i32, nums: Option<i32>, complete: Option<bool>) -> ProtocolEvent {
            ProtocolEvent::DungeonObjective {
                target_id,
                nums,
                complete,
            }
        }

        fn objective_removed(target_id: i32) -> ProtocolEvent {
            ProtocolEvent::DungeonObjectiveRemoved { target_id }
        }

        fn var(name: &str, value: i32) -> ProtocolEvent {
            ProtocolEvent::DungeonVar {
                name: name.to_string(),
                value,
            }
        }

        /// issue #139: the hard constraint that makes every path above
        /// additive rather than a replacement — a session that never sees
        /// `0x17`/`0x18` never sets `dungeon_state`, so
        /// `dungeon_objective_still_running` (§8's gate) always reads
        /// `false` and a recognized boss's death ends the fight exactly as
        /// it always has, with no dungeon packets in the picture at all.
        #[test]
        fn boss_death_still_ends_the_fight_when_no_dungeon_events_have_ever_arrived() {
            let mut m = Meter::new();
            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        /// §2: `Playing` is authoritative even mid-fight — it forces a
        /// fresh encounter outright, the same as a manual reset.
        #[test]
        fn dungeon_state_playing_forces_a_fresh_encounter() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            assert_eq!(m.snapshot(1_000).total_damage, 100);

            let reason = m.apply(&dungeon_state(EDungeonState::Playing));

            assert_eq!(reason, Some(ResetReason::DungeonStarted));
            assert_eq!(m.snapshot(1_000).total_damage, 0);
        }

        /// §3: timestamped by `last_event_ms` (the last real damage), not
        /// "now" -- the `DungeonState::End` packet itself can arrive well
        /// after the hitting actually stopped, the same rule the `Scene`
        /// arm's `SceneChanged` latch already follows.
        #[test]
        fn dungeon_state_end_latches_the_fight_end_at_the_last_damage_time() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.apply(&dungeon_state(EDungeonState::End));

            assert_eq!(m.fight_end_ms(), Some(1_000));
        }

        /// §3's other member of the arm.
        #[test]
        fn dungeon_state_settlement_latches_the_fight_end_at_the_last_damage_time() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.apply(&dungeon_state(EDungeonState::Settlement));

            assert_eq!(m.fight_end_ms(), Some(1_000));
        }

        /// §4: back to open world clears the dungeon tracking outright, so
        /// the §8 gate a still-incomplete objective was holding open lifts
        /// immediately and an ordinary boss death ends the fight again.
        #[test]
        fn dungeon_state_null_clears_the_gate_so_boss_death_ends_normally_again() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&dungeon_state(EDungeonState::Null));

            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        /// §5: a genuinely new objective (different id, fresh at zero
        /// progress, not complete) transitions `current_objective_id`
        /// without resetting anything -- it has not yet moved back onto
        /// `first_objective_id`, so §6 does not fire. There is no public
        /// getter for `current_objective_id`, so the transition is
        /// witnessed indirectly through §8: only once the *new* current
        /// objective (200) is marked complete does a recognized boss's
        /// death stop being gated -- proving the meter really is now
        /// tracking 200, not still 100.
        #[test]
        fn a_new_objective_transitions_current_without_resetting() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            let first = m.apply(&objective(100, Some(0), Some(false)));
            assert_eq!(first, None);
            let second = m.apply(&objective(200, Some(0), Some(false)));
            assert_eq!(second, None);

            m.apply(&objective(200, None, Some(true)));
            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        /// §6 (issue #210's own case): the id that started the raid
        /// reappearing as current, after the instance had already moved
        /// off it onto a second objective, means one raid boss died while
        /// others are unbeaten -- a fresh encounter inside the same
        /// instance. Witnessed directly: the reset clears the accumulated
        /// damage.
        #[test]
        fn objective_returning_to_the_first_id_after_moving_off_it_starts_a_fresh_encounter() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&dmg(1, 5_000, 1_000));
            assert_eq!(m.snapshot(1_000).total_damage, 5_000);

            m.apply(&objective(200, Some(0), Some(false)));
            let reason = m.apply(&objective(100, Some(0), Some(false)));

            assert_eq!(reason, Some(ResetReason::DungeonStarted));
            assert_eq!(m.snapshot(1_000).total_damage, 0);
        }

        /// §6's guard against false-triggering on an instance's own
        /// opening objective: the very first `DungeonObjective` this
        /// instance ever reports also "equals `first_objective_id`" (it is
        /// what just established it), but `current_objective_id` was
        /// `None` going in, so it must not read as a reset.
        #[test]
        fn the_first_ever_objective_does_not_itself_trigger_the_raid_boss_reset() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&dmg(1, 5_000, 1_000));

            let reason = m.apply(&objective(100, Some(0), Some(false)));

            assert_eq!(reason, None);
            assert_eq!(m.snapshot(1_000).total_damage, 5_000);
        }

        /// §7: ZDPS's documented completion fallback -- treated exactly
        /// like §3's `End`/`Settlement` latch, timestamped the same way.
        #[test]
        fn is_finish_target_var_with_a_nonzero_value_latches_the_fight_end() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.apply(&var("IsFinishTarget", 1));

            assert_eq!(m.fight_end_ms(), Some(1_000));
        }

        #[test]
        fn is_finish_target_var_with_a_zero_value_does_not_latch() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.apply(&var("IsFinishTarget", 0));

            assert_eq!(m.fight_end_ms(), None);
        }

        /// Every other var name is decoded (spec "New events") but the
        /// meter acts on `IsFinishTarget` only.
        #[test]
        fn dungeon_vars_other_than_is_finish_target_are_ignored() {
            let mut m = Meter::new();
            m.apply(&dmg(1, 100, 1_000));
            m.apply(&var("music_value", 999));

            assert_eq!(m.fight_end_ms(), None);
        }

        /// §8 (issue #210's case): while the dungeon confirms it is still
        /// running and the current objective is known and incomplete, a
        /// recognized boss dying is a phase of the instance, not the fight
        /// ending -- unlike the plain no-dungeon-events case above (and
        /// `fight_end::a_recognized_boss_dying_ends_the_fight_immediately`
        /// outside this module), which both still end on the kill.
        #[test]
        fn a_boss_death_does_not_end_the_fight_while_the_objective_is_still_incomplete() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Active);
        }

        /// §8's counterpart: once the current objective is marked
        /// complete, the gate lifts and the same kill ends the fight the
        /// ordinary way.
        #[test]
        fn a_boss_death_ends_the_fight_once_the_objective_completes() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&objective(100, None, Some(true)));
            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        /// PR #226 review, finding 1: a raid retried in place re-sends
        /// `Playing` with no scene change, no reconnect and no `Null` in
        /// between, so `Playing` is the only chance to drop the failed
        /// attempt's objective tracking. Witnessed through §6: with the
        /// old attempt's `first_objective_id` (100) and
        /// `current_objective_id` (200) carried over, the retry's own
        /// opening objective -- the same id 100, since it is the same
        /// raid -- looks exactly like "moved off the first objective and
        /// came back to it" and wrongly wipes the fresh attempt.
        #[test]
        fn a_raid_retried_in_place_does_not_inherit_the_previous_attempts_objectives() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Playing));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&objective(200, Some(0), Some(false)));

            m.apply(&dungeon_state(EDungeonState::Playing));
            m.apply(&dmg(1, 5_000, 1_000));
            let reason = m.apply(&objective(100, Some(0), Some(false)));

            assert_eq!(reason, None);
            assert_eq!(m.snapshot(1_000).total_damage, 5_000);
        }

        /// PR #226 review, finding 3: an objective can be reported for
        /// the first time already complete (a step the party finished
        /// before the meter was looking, or one the instance hands out
        /// pre-satisfied). That fails §5's new-objective signature, but
        /// it is still the objective that opened this instance, so it has
        /// to establish `first_objective_id` -- otherwise §6 never arms
        /// and the *second* objective (200) wrongly becomes "first",
        /// which makes the genuine return to 100 below read as ordinary
        /// progress instead of the raid-boss reset it is.
        #[test]
        fn an_objective_already_complete_on_its_first_sighting_still_establishes_the_first_id() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(true)));
            m.apply(&objective(200, Some(0), Some(false)));
            m.apply(&dmg(1, 5_000, 1_000));
            assert_eq!(m.snapshot(1_000).total_damage, 5_000);

            let reason = m.apply(&objective(100, Some(0), Some(false)));

            assert_eq!(reason, Some(ResetReason::DungeonStarted));
            assert_eq!(m.snapshot(1_000).total_damage, 0);
        }

        /// PR #226 review, finding 2: the game can drop an objective out
        /// of its table without ever marking it complete. Nothing else
        /// clears `current_objective_id`, so before removals were
        /// propagated at all this left §8's gate stuck open for the rest
        /// of the instance -- no boss death could end a fight in it
        /// again. A removal is not a completion, so the objective is
        /// forgotten rather than marked done; the gate lifts because the
        /// current objective is unknown again.
        #[test]
        fn an_objective_removed_without_completing_stops_gating_the_fight_end() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(100, Some(0), Some(false)));
            m.apply(&objective_removed(100));

            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }

        /// The removal's other half: an id the meter is not tracking is
        /// simply nothing to do, and must not disturb the objective that
        /// *is* current (200 here still gates §8).
        #[test]
        fn removing_an_untracked_objective_leaves_the_current_one_alone() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&objective(200, Some(0), Some(false)));
            m.apply(&objective_removed(999));

            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Active);
        }

        /// §8 does not gate at all when the current objective is unknown
        /// (never reported), even with a dungeon confirmed in progress --
        /// there is no evidence it is still running, so the heuristic
        /// takes over exactly as it does out in the open world.
        #[test]
        fn a_boss_death_ends_the_fight_when_no_objective_has_been_reported_yet() {
            let mut m = Meter::new();
            m.apply(&dungeon_state(EDungeonState::Active));
            m.apply(&boss_hit(BOSS_UID, 1_000, false));
            m.apply(&hp(BOSS_UID, BOSS, 1_000));
            m.apply(&boss_hit(BOSS_UID, 2_000, true));

            assert_eq!(m.fight_state(2_100), FightState::Ended);
        }
    }
}
