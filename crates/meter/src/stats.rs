//! Per-player stats and the UI-facing snapshot read model (plan §T2.1).

use crate::event::Class;
use std::collections::HashMap;

/// Per-skill accumulator (issue #16). Keyed by the raw wire skill id — BPSR
/// self-reports a stable id on every sub-hit, so no normalisation, no
/// divide-by-100, no parent/sub-skill remap.
#[derive(Debug, Clone, Default)]
pub struct SkillStats {
    pub total_damage: i64,
    pub hits: u64,
    pub crit_hits: u64,
    pub crit_damage: i64,
    pub lucky_hits: u64,
    pub lucky_damage: i64,
    /// D7: running max of *crit* hit values. No upstream tracker keeps this
    /// (resonance-logs, bpsr-logs and BPSR-ZDPS all sum but never max a
    /// crit), so it is ours to maintain — one comparison per crit event.
    pub max_crit: i64,
    /// Sum of every `DamageEvent` on this skill whose `kind` was
    /// `DamageKind::Absorbed` (issue #338) — a target's shield fully soaked
    /// the hit. Kept out of `total_damage`/DPS entirely; this is its own
    /// channel, not a damage sub-total.
    pub absorbed_total: i64,
    /// Sum of every `DamageEvent` on this skill whose `kind` was
    /// `DamageKind::Immune` (issue #338). Usually stays `0` — an immune hit
    /// is typically reported with `value == 0` — but this still gets its
    /// own channel rather than assuming that always holds (see
    /// `DamageKind::Immune`'s doc comment).
    pub immune_total: i64,
}

impl SkillStats {
    /// Folds `other` into `self` (issue #245). Used to build the "Skill
    /// dealt" view, which is the union of a player's outgoing damage and
    /// outgoing healing under one skill id — a skill that both damages and
    /// heals contributes to both accumulators, and the dealt view wants the
    /// sum rather than either half.
    ///
    /// `max_crit` takes the larger of the two rather than summing, being a
    /// running max and not a total.
    pub fn merge(&mut self, other: &SkillStats) {
        self.total_damage += other.total_damage;
        self.hits += other.hits;
        self.crit_hits += other.crit_hits;
        self.crit_damage += other.crit_damage;
        self.lucky_hits += other.lucky_hits;
        self.lucky_damage += other.lucky_damage;
        self.max_crit = self.max_crit.max(other.max_crit);
        self.absorbed_total += other.absorbed_total;
        self.immune_total += other.immune_total;
    }
}

/// Per-buff-type uptime accumulator (issue #267), keyed by `base_id` on
/// `PlayerStats::buffs` — the Buff tab. Built only from **closed**
/// apply→remove intervals (`Meter::apply_buff_remove`); a buff still active
/// when a snapshot is read contributes nothing until it closes. This is an
/// acceptable v1 undercount, not a wrong number: the alternative (counting
/// a still-open interval up to "now") is a documented follow-up, not
/// implemented here.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuffStats {
    /// Summed over every closed interval this encounter, milliseconds.
    pub total_uptime_ms: u64,
    /// How many apply→remove cycles have closed for this buff this
    /// encounter — not the same as how many `BuffApply`-shaped wire events
    /// arrived (a stack/refresh mid-interval does not open a new one).
    pub apply_count: u32,
}

/// One in-flight buff instance (issue #267), tracked on
/// `PlayerStats::active_buffs` between an apply-like wire event and the
/// remove-like one that closes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveBuff {
    /// The buff's definition/template id, once known. Starts however the
    /// opening apply event resolved it (`ProtocolEvent::BuffApply::base_id`
    /// — frequently `None`, see that variant's doc comment) and is
    /// backfilled by a later apply-like event for the same `buff_uuid` that
    /// does carry one, since a stack/refresh can supply it even when the
    /// original application didn't.
    pub(crate) base_id: Option<i32>,
    pub(crate) start_ms: u64,
    /// How many stack layers this instance currently holds: 1 when it opens,
    /// grown by a `StackLayer` apply and shrunk by a `RemoveLayer` remove
    /// (see `ProtocolEvent::BuffRemove::removes_layer`). The interval closes
    /// when it reaches zero, so a stacking buff that sheds one layer stays
    /// up instead of losing the rest of its uptime.
    pub(crate) layers: u32,
}

#[derive(Debug, Clone)]
pub struct PlayerStats {
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power); `None` until a packet carrying
    /// it (attrs `FIGHT_POINT`, or `SyncContainerData.fight_point`) has been
    /// seen for this player (issue #15).
    pub ability_score: Option<u32>,
    /// Season strength; `None` until a packet carrying it (attrs
    /// `SEASON_STRENGTH`) has been seen for this player (issue #15).
    pub season_strength: Option<u32>,
    /// The two equipped Imagines, as opaque skill ids; `None` until a `0x74`
    /// packet has been seen for this player, `Some([None, None])` once one
    /// has been seen but no known Imagine was found (issue #33). See
    /// `event::PlayerInfo::imagines` for the full merge-rule doc.
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    pub imagines: Option<[Option<i32>; 2]>,
    /// Each equipped slot's tier (issues #169/#170). See
    /// `event::PlayerInfo::imagine_tiers` for the full merge-rule doc —
    /// mirrors `imagines`'s `None`/`Some` semantics one-for-one.
    pub imagine_tiers: Option<[Option<i32>; 2]>,
    pub total_damage: i64,
    pub hits: u64,
    pub crit_hits: u64,
    pub crit_damage: i64,
    pub lucky_hits: u64,
    pub lucky_damage: i64,
    /// Sum of every hit this player dealt whose `kind` was
    /// `DamageKind::Absorbed` (issue #338) — the target's shield fully
    /// soaked it. Excluded from `total_damage`/DPS on purpose; see
    /// `SkillStats::absorbed_total`, the per-skill breakdown this mirrors
    /// at the player level.
    pub absorbed_total: i64,
    /// Sum of every hit this player dealt whose `kind` was
    /// `DamageKind::Immune` (issue #338). See `SkillStats::immune_total`.
    pub immune_total: i64,
    /// This player's current total shield value, sourced from
    /// `PlayerInfo::shield` (`AttrShieldList`, issue #338). `None` until a
    /// packet carrying the attr has been seen for this player this
    /// encounter — see that field's doc comment for the `None`-vs-`Some(0)`
    /// distinction, which carries through unchanged here. Not accumulated
    /// like the totals above: this is a live gauge, overwritten by every
    /// packet that carries it, not summed across the encounter.
    pub shield: Option<i64>,
    /// Times this player died this encounter, per `DamageEvent::is_dead`
    /// (issue #49). This counts the *victim*, not the attacker — see
    /// `Meter::apply_damage`.
    pub deaths: u32,
    /// Whether this player is up *right now*, as opposed to `deaths > 0`
    /// which only ever asks whether they have died at some point this
    /// encounter (issue #212). Cleared by `Meter::record_death`; set again
    /// by `Meter::apply_damage` on the next event this player *acts* in,
    /// heal-typed events included — the decode layer delivers no
    /// player-HP signal, so acting at all is the only revive evidence
    /// available, and a support who never deals damage has nothing else
    /// to offer. Starts `true`: a freshly-seen row (an attacker, or a
    /// roster preload) has not been observed dead.
    ///
    /// Written only through [`Self::set_alive`], which orders the
    /// transitions by the event clock — see there.
    pub(crate) alive: bool,
    /// Per-skill breakdown (issue #16), keyed by raw skill id. Emitted in
    /// every snapshot rather than behind a subscription: resonance-logs
    /// gates its equivalent because it serialises to a webview per tick;
    /// ours is an in-process clone of at most a raid's players times a few
    /// dozen skills, and `SkillRow` carries the raw id (name resolved at
    /// draw time from the static table), so nothing allocates per skill per
    /// tick.
    pub skills: HashMap<i32, SkillStats>,
    /// Per-skill breakdown of this player's *outgoing healing* (issue
    /// #245), keyed the same way `skills` is. Kept as its own map rather
    /// than folded into `skills` because the damage view must stay a
    /// damage view: a healer's output would otherwise inflate every DPS
    /// figure that divides `total_damage` by the fight clock.
    ///
    /// `SkillStats::total_damage` here holds healing done — the accumulator
    /// is amount-agnostic and reusing it keeps one set of per-skill
    /// arithmetic (`skill_row_from_stats`) for every tab.
    pub heals: HashMap<i32, SkillStats>,
    /// Per-skill breakdown of what this player *received* (issue #245):
    /// damage taken and healing received alike, keyed by the raw skill id
    /// of the effect that landed on them. This is the one accumulator keyed
    /// off `DamageEvent::target_uid` rather than `attacker_uid`.
    pub incoming: HashMap<i32, SkillStats>,
    /// How many times this player has *begun* each skill (issue #245),
    /// keyed by raw skill id — the Skill casts tab. A count, not a
    /// `SkillStats`: a cast carries no amount, no crit bit and no target,
    /// so there is nothing else about it to accumulate.
    pub casts: HashMap<i32, u64>,
    /// Sum of `heals`' amounts, i.e. the denominator for the Heal tab's
    /// `% Heal` column. Tracked alongside the map for the same reason
    /// `total_damage` is: the share column needs a player total that does
    /// not depend on iterating the map on every snapshot.
    pub total_heal: i64,
    /// Sum of `incoming`'s amounts — the "Skill received" tab's share
    /// denominator.
    pub total_incoming: i64,
    /// Timestamp (event clock, not wall time) of the last death counted for
    /// this player, used to debounce a retransmitted/duplicated delta packet
    /// from double-counting a single death (issue #49). `pub(crate)`, not
    /// part of the display DTO (`PlayerRow`) — see `DEATH_DEBOUNCE_MS`.
    pub(crate) last_death_ms: Option<u64>,
    /// Timestamp (event clock) of the transition `alive` currently
    /// records, i.e. how fresh that evidence is. `pub(crate)`, not part of
    /// the display DTO (`PlayerRow`) — see [`Self::set_alive`].
    pub(crate) alive_as_of_ms: Option<u64>,
    /// Total time this player has spent down this encounter, in event-clock
    /// milliseconds, summed over every death→revive interval that has
    /// *closed* (issue #254). The interval still open — a player who is
    /// down right now — is added at read time by [`Self::dead_ms_as_of`],
    /// so this field never depends on a poll.
    ///
    /// This is an **estimate, biased high**, and the display labels it as
    /// one. Only the death edge is observed: `Meter::record_death` has a
    /// real `DamageEvent::is_dead` timestamp. The revive edge is inferred
    /// from the player's next *acted* event (see [`Self::alive`]), because
    /// the decode layer delivers no player-HP or revive signal, so a player
    /// who is back up but not yet acting — repositioning, out of range, a
    /// support between casts — still counts as down until their next hit or
    /// heal. Should a direct revive signal ever be decoded (issue #254's PR
    /// body records where the reference trackers find one), it lands here
    /// with no change to this accounting: every transition already goes
    /// through the one [`Self::set_alive`] call below.
    pub(crate) dead_ms: u64,
    /// Event-clock time of the death that opened the interval currently
    /// running, or `None` while this player is up (issue #254).
    ///
    /// Deliberately *not* `alive_as_of_ms`, which every `set_alive` call
    /// rewrites: a retransmitted death packet arriving while this player is
    /// already down advances that field, and reusing it as the interval's
    /// start would silently shorten the death by the gap between the two
    /// copies. This one is written on the up→down edge only.
    pub(crate) dead_since_ms: Option<u64>,
    /// Per-buff-type breakdown (issue #267), keyed by `base_id` — the Buff
    /// tab. See [`BuffStats`] for what is (and isn't) counted.
    pub buffs: HashMap<i32, BuffStats>,
    /// In-flight buff instances, keyed by wire `buff_uuid` (issue #267).
    /// `pub(crate)`, not part of the display DTO (`PlayerRow`) — see
    /// [`ActiveBuff`]. `buff_uuid` alone is enough to key this map: each
    /// `PlayerStats` is already scoped to one host, and a host cannot have
    /// two simultaneously-active buff instances sharing one `buff_uuid`.
    pub(crate) active_buffs: HashMap<i32, ActiveBuff>,
}

impl PlayerStats {
    pub fn new(uid: i64) -> Self {
        Self {
            uid,
            name: None,
            class: None,
            ability_score: None,
            season_strength: None,
            imagines: None,
            imagine_tiers: None,
            total_damage: 0,
            hits: 0,
            crit_hits: 0,
            crit_damage: 0,
            lucky_hits: 0,
            lucky_damage: 0,
            absorbed_total: 0,
            immune_total: 0,
            shield: None,
            deaths: 0,
            alive: true,
            skills: HashMap::new(),
            heals: HashMap::new(),
            incoming: HashMap::new(),
            casts: HashMap::new(),
            total_heal: 0,
            total_incoming: 0,
            last_death_ms: None,
            alive_as_of_ms: None,
            dead_ms: 0,
            dead_since_ms: None,
            buffs: HashMap::new(),
            active_buffs: HashMap::new(),
        }
    }

    /// Records that this player is (or is not) up as of `timestamp_ms`
    /// (issue #212).
    ///
    /// Ordered by the event clock, the way `EnemyState::last_damaged_ms`
    /// is and for the same reason: a packet older than the transition
    /// already recorded is dropped rather than allowed to flip the bit
    /// back. Without that a hit retransmitted *behind* the death packet it
    /// preceded would read as a battle rez and hide a real wipe — a
    /// failure mode the cumulative `deaths` counter this replaced could
    /// not have, being monotonic (PR #224 review, finding 3).
    ///
    /// Equal timestamps still write, so events sharing one packet's clock
    /// apply in arrival order — which is what makes a killing blow a
    /// player deals to themselves land *after* the swing that carried it
    /// and leave them down.
    pub(crate) fn set_alive(&mut self, alive: bool, timestamp_ms: u64) {
        if self.alive_as_of_ms.is_some_and(|last| timestamp_ms < last) {
            return;
        }
        // Issue #254: the two *edges* — and only the edges — move the dead
        // clock. A repeated `set_alive(false)` while already down (the
        // retransmitted death packet `Meter::record_death` debounces) leaves
        // the open interval's start where the first copy put it, and a
        // repeated `set_alive(true)` adds nothing.
        match (self.alive, alive) {
            (true, false) => self.dead_since_ms = Some(timestamp_ms),
            (false, true) => {
                if let Some(since) = self.dead_since_ms.take() {
                    self.dead_ms += timestamp_ms.saturating_sub(since);
                }
            }
            _ => {}
        }
        self.alive = alive;
        self.alive_as_of_ms = Some(timestamp_ms);
    }

    /// Estimated total time this player has spent down this encounter as of
    /// `now_ms`, closed intervals plus the one still open (issue #254).
    ///
    /// A death still open at the end of the encounter — the player never
    /// revived before the pull ended, wiped or reset — is counted **up to
    /// the encounter's end**, which is what `Meter::snapshot` passes here:
    /// its `effective_now_ms`, i.e. the fight's end once the fight has
    /// ended and the caller's clock while it is live. That matches the
    /// reference (`Skills.xaml.cs` totals death time over
    /// `BeginTime..EndTime`) and keeps a wipe reading as "down for the rest
    /// of the fight" rather than as no death time at all. It also means a
    /// live open death ticks upward between snapshots, exactly like the
    /// fight timer beside it.
    ///
    /// See [`Self::dead_ms`] for why the total is an estimate.
    pub(crate) fn dead_ms_as_of(&self, now_ms: u64) -> u64 {
        self.dead_ms
            + self
                .dead_since_ms
                .map_or(0, |since| now_ms.saturating_sub(since))
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
    /// Ability score (a.k.a. combat power); `None` when no packet carrying
    /// it has been seen for this player yet (issue #15).
    pub ability_score: Option<u32>,
    /// Season strength; `None` when no packet carrying it has been seen for
    /// this player yet (issue #15).
    pub season_strength: Option<u32>,
    /// The two equipped Imagine slots, as opaque skill ids resolved to
    /// display data only by `crates/app/src/imagines.rs` (issue #33). Always
    /// exactly two slots — `None` per-slot means empty/unknown, never a
    /// missing packet (that distinction lives on `PlayerStats::imagines`).
    // IMAGINE-TAKEDOWN: part of the imagines field chain (see plan D4 #5).
    pub imagines: [Option<i32>; 2],
    /// Each equipped slot's tier (issues #169/#170), positionally paired
    /// with `imagines` — always exactly two slots, `None` per-slot meaning
    /// empty/unresolved/unknown, same convention as `imagines` itself.
    pub imagine_tiers: [Option<i32>; 2],
    pub damage: i64,
    pub dps: f64,
    pub share_pct: f32,
    pub crit_pct: f32,
    pub lucky_pct: f32,
    pub hits: u64,
    /// Sum of this player's `SkillStats::absorbed_total` across every skill
    /// (issue #338) — a shield fully soaking one of their hits. Excluded
    /// from `damage`/`dps` above; see `PlayerStats::absorbed_total`.
    pub absorbed_total: i64,
    /// Sum of this player's `SkillStats::immune_total` across every skill
    /// (issue #338). See `PlayerStats::immune_total`.
    pub immune_total: i64,
    /// This player's current total shield value (issue #338). Mirrors
    /// `PlayerStats::shield` — `None` means unseen this encounter, not
    /// "no shield right now" (that's `Some(0)`).
    pub shield: Option<i64>,
    /// Times this player died this encounter (issue #49). See
    /// `PlayerStats::deaths`.
    pub deaths: u32,
    /// Estimated total time this player spent dead this encounter, in
    /// milliseconds (issue #254) — see `PlayerStats::dead_ms` for how it is
    /// accumulated and why it is an estimate rather than an exact figure
    /// like `deaths`.
    ///
    /// `None` means *not measured*, not zero: a row replayed out of the
    /// history database predates the column that would carry it, and the
    /// UI hides the pill for those rather than claiming a saved encounter
    /// had no deaths on the floor.
    pub dead_ms: Option<u64>,
    /// This player's per-skill breakdown, damage-descending (issue #16). One
    /// row per raw skill id — the reference's sub-skill "short name"
    /// grouping has no analogue in BPSR's flat ids, so there is deliberately
    /// no expander/grouping tier here (D12).
    pub skills: Vec<SkillRow>,
    /// The Heal tab's rows (issue #245): this player's outgoing healing per
    /// skill, healing-descending. `SkillRow::damage`/`share_pct` carry the
    /// heal amount and its share of this player's healing — `SkillRow` is
    /// deliberately reused verbatim across every tab so one set of
    /// per-skill arithmetic, one sort path and one history record shape
    /// serve them all.
    pub heals: Vec<SkillRow>,
    /// The "Skill dealt" tab's rows (issue #245): everything this player
    /// put out, damage and healing merged under one skill id,
    /// amount-descending.
    pub dealt: Vec<SkillRow>,
    /// The "Skill received" tab's rows (issue #245): everything that landed
    /// *on* this player, damage taken and healing received alike,
    /// amount-descending.
    pub received: Vec<SkillRow>,
    /// The Skill casts tab's rows (issue #245): how often this player began
    /// each skill, cast-count-descending. Reuses `SkillRow` like every
    /// other breakdown — `hits` is the cast count and `hits_per_min` the
    /// rate; every amount-shaped field stays `0`, because a cast has no
    /// amount and the tab shows no amount column
    /// (`skills::SkillTab::columns`).
    pub casts: Vec<SkillRow>,
    /// The Buff tab's rows (issue #267): this player's per-buff-type
    /// uptime, uptime-descending. `SkillRow` is reused verbatim, like every
    /// other breakdown tab (`hits` is the apply count and `avg` the mean
    /// duration per application, milliseconds; `crit_pct`/`max_crit`/
    /// `avg_crit`/`avg_white`/`crit_hits`/`hits_per_min` stay `0`, unused by
    /// `skills::SkillTab::Buff`'s column set) — `skill_id` carries the
    /// buff's `base_id`, and `damage`/`share_pct` carry total/percentage
    /// uptime. See [`BuffStats`] for what is (and isn't) counted.
    pub buffs: Vec<SkillRow>,
}

/// One row of a player's skill breakdown (issue #16). This is the contract
/// the skills-window UI consumes — do not change it without updating that
/// consumer.
#[derive(Debug, Clone)]
pub struct SkillRow {
    /// Raw wire skill id; the display name is resolved at draw time via
    /// `tables::skill_name`, so no `String` is allocated per skill per tick.
    pub skill_id: i32,
    pub damage: i64,
    /// Share of this *player's* damage, not the encounter's.
    pub share_pct: f32,
    pub crit_pct: f32,
    pub max_crit: i64,
    /// Mean crit hit; `0.0` when this skill has never crit.
    pub avg_crit: f64,
    /// D6: mean non-crit hit, `(total_damage - crit_damage) / (hits -
    /// crit_hits)`. Lucky is deliberately *not* excluded — lucky and crit
    /// are orthogonal flags in BPSR, so a lucky non-crit hit is still a
    /// white hit. `0.0` when every hit crit, matching the reference's
    /// literal `0`.
    pub avg_white: f64,
    pub avg: f64,
    pub hits: u64,
    pub crit_hits: u64,
    /// D8: hits per minute over `Meter::snapshot`'s shared
    /// `dps_duration_ms`, so a skill's rate can never disagree with the
    /// row's own DPS window.
    pub hits_per_min: f64,
    /// This skill's `SkillStats::absorbed_total` (issue #338) — not part of
    /// `damage` above.
    pub absorbed: i64,
    /// This skill's `SkillStats::immune_total` (issue #338).
    pub immune: i64,
}

/// What the meter believes is being fought, as far as the packet stream reveals
/// it. Every field is independently unknown-able: a scene id can arrive before
/// any boss is identified, and a boss can be identified in an unnamed scene.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncounterInfo {
    pub boss_monster_id: Option<u32>,
    pub boss_name: Option<&'static str>,
    /// Whether `boss_monster_id` is a recognized boss (issue #42), i.e.
    /// whether the header should ever show a name for it. Kept separate from
    /// `boss_name.is_some()` because a boss can be in `tables::BOSS_MONSTER_IDS`
    /// without a resolved name (the two vendored lists aren't guaranteed to
    /// agree) — `is_boss` is the single source of truth for the display
    /// gate, `boss_name` for the text.
    pub is_boss: bool,
    pub scene_id: Option<u32>,
    pub scene_name: Option<&'static str>,
    /// The current dungeon scene's final boss (issue #125), when it is one of
    /// the curated single-boss dungeons in `tables::SCENE_FINAL_BOSSES`
    /// (issue #201 — this used to be learned at runtime and cached to disk).
    /// Independent of `boss_monster_id`/`boss_name`/`is_boss` above, which
    /// remain the raw facts about the currently-selected target;
    /// `encounter_title` in `crates/app/src/ui.rs` falls back to this field
    /// so a mid-dungeon mech (or even a genuine mid-dungeon boss) never
    /// displaces the dungeon's final boss name.
    ///
    /// `None` for a dungeon nobody has curated — most of them — and `None` in
    /// a scene that can present more than one *selectable* boss (issue #150),
    /// see `multi_boss_scene` below and `Meter::snapshot` for the suppression
    /// itself.
    pub scene_boss_name: Option<&'static str>,
    /// Whether this scene is known to offer more than one separately
    /// selectable boss (issue #150): a raid where the party picks which of
    /// three bosses to pull. True for the curated raid scenes
    /// (`phase::is_boss_select_scene`), which is the only source — the meter
    /// cannot tell a raid's selections from an ordinary dungeon's boss order
    /// by observation. Drives `encounter_title`'s
    /// "Select a boss" placeholder in `crates/app/src/ui.rs`: with nothing
    /// engaged there is genuinely no target *yet*, as opposed to the
    /// no-target-at-all case "No target" names.
    pub multi_boss_scene: bool,
}

/// Cheap, immutable snapshot of the current encounter, sorted by damage
/// descending.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub duration_ms: u64,
    pub total_damage: i64,
    /// Aggregate DPS over the same window used for each row's `dps` (see
    /// `Meter::snapshot`), not `duration_ms` — keeping the header and rows on
    /// separate denominators lets them diverge (e.g. a huge spike on the
    /// first tick, or the header decaying while idle).
    pub total_dps: f64,
    /// Sum of every row's `absorbed_total` (issue #338) — total shield
    /// damage this encounter, kept out of `total_damage`/`total_dps`.
    pub total_absorbed: i64,
    /// Sum of every row's `immune_total` (issue #338).
    pub total_immune: i64,
    pub rows: Vec<PlayerRow>,
    /// What is being fought, if the packet stream has revealed it (issue #9
    /// slice 2).
    pub encounter: EncounterInfo,
    /// Whether the packet-capture thread is still alive, as far as the
    /// caller publishing this snapshot knows (pipeline-robustness audit,
    /// finding 1). The meter itself has no way to know this — it only ever
    /// sees the events it is handed — so every `Snapshot` this crate builds
    /// sets it `true`; `bpsr_app::pipeline` is the one place that ever
    /// flips it to `false`, once its capture-event channel disconnects, so
    /// the overlay can tell "no data because nothing is happening" apart
    /// from "no data because the thing that would produce it is gone."
    pub capture_alive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #254: only the two edges move the dead clock, and the total
    /// is the sum of the closed intervals.
    #[test]
    fn set_alive_edges_accumulate_dead_time() {
        let mut s = PlayerStats::new(1);
        s.set_alive(false, 1_000);
        s.set_alive(true, 4_000);
        s.set_alive(false, 10_000);
        s.set_alive(true, 10_500);
        assert_eq!(s.dead_ms, 3_500);
        assert_eq!(s.dead_since_ms, None);
        assert_eq!(s.dead_ms_as_of(60_000), 3_500);
    }

    /// A second death packet for a player who is already down (the
    /// retransmission `Meter::record_death` debounces) leaves the interval's
    /// start where the first one put it.
    #[test]
    fn a_repeated_death_keeps_the_first_start() {
        let mut s = PlayerStats::new(1);
        s.set_alive(false, 1_000);
        s.set_alive(false, 1_400);
        s.set_alive(true, 5_000);
        assert_eq!(s.dead_ms, 4_000);
    }

    /// A repeated proof of life adds nothing — every acted event calls
    /// `set_alive(true, _)`, so this runs constantly in a real fight.
    #[test]
    fn a_repeated_revive_adds_nothing() {
        let mut s = PlayerStats::new(1);
        s.set_alive(false, 1_000);
        s.set_alive(true, 5_000);
        s.set_alive(true, 6_000);
        s.set_alive(true, 7_000);
        assert_eq!(s.dead_ms, 4_000);
    }

    #[test]
    fn dead_ms_as_of_adds_the_interval_still_open() {
        let mut s = PlayerStats::new(1);
        s.set_alive(false, 1_000);
        s.set_alive(true, 3_000);
        s.set_alive(false, 8_000);
        assert_eq!(s.dead_ms, 2_000, "only the closed interval is stored");
        assert_eq!(s.dead_ms_as_of(9_500), 3_500);
        assert_eq!(
            s.dead_ms_as_of(7_000),
            2_000,
            "a clock behind the open death saturates instead of underflowing"
        );
    }

    /// The out-of-order clamp covers the dead clock too: a transition older
    /// than the one already recorded is dropped whole.
    #[test]
    fn a_stale_transition_moves_neither_the_bit_nor_the_dead_clock() {
        let mut s = PlayerStats::new(1);
        s.set_alive(false, 5_000);
        s.set_alive(true, 4_000);
        assert!(!s.alive);
        assert_eq!(s.dead_ms, 0);
        assert_eq!(s.dead_ms_as_of(9_000), 4_000);
    }

    #[test]
    fn a_player_who_never_died_has_no_dead_time() {
        let s = PlayerStats::new(1);
        assert_eq!(s.dead_ms_as_of(600_000), 0);
    }

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
