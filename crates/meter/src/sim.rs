//! Synthetic-combat simulator (issue #40): generates deterministic
//! `ProtocolEvent` streams that exercise `Meter` the same way a real
//! decoded packet stream would, so the meter/overlay can be driven and
//! visually inspected without the game client or a recorded packet
//! capture. See `crates/meter/src/bin/sim.rs` for a runnable viewer and
//! `crates/protocol/src/bin/inspect-replay.rs` for the complementary
//! real-capture-replay tool this does *not* replace.
//!
//! ## Design decisions (resolving issue #40's open questions)
//!
//! - **Feeds `Meter::apply` directly with `ProtocolEvent`s**, not the
//!   `bpsr-protocol` decoder: the meter is the unit under inspection here,
//!   and going through the wire-decode path would pull GPU/GUI/OS-specific
//!   capture plumbing into what should stay a pure-logic dev tool.
//! - **Always-compiled `pub mod sim`**, not a cargo feature: `bpsr-meter`
//!   has no GUI/GPU deps to begin with (`#![forbid(unsafe_code)]`, pure
//!   logic crate), so gating this behind a feature would only add
//!   `--features`-juggling for zero build-cost benefit. `crates/app` never
//!   depends on this module, so it costs that crate nothing either way.
//! - **Hand-rolled PRNG** (`Rng`, xorshift64*), not the `rand` crate: no new
//!   third-party dependency, and a simulator only needs "looks varied and
//!   reproduces exactly from a seed," not cryptographic quality.
//! - **Named preset scenarios** rather than a scriptable format: three
//!   presets (`NormalParty`, `Raid20`, `DisconnectRejoin`) cover the use
//!   cases named in the issue (manual visual inspection, a 20-player raid,
//!   a reset edge case) without the design surface of a scenario DSL, which
//!   nothing in this repo currently needs.

use crate::event::{DamageEvent, EntityKind, PlayerInfo, ProtocolEvent};

/// Deterministic xorshift64* PRNG. Seed must be nonzero (a zero seed is
/// remapped to a fixed nonzero constant) — xorshift's all-zero state is a
/// fixed point that never advances.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform integer in `[lo, hi)`. `hi` must be `> lo`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }

    /// `true` with probability `pct_chance / 100`.
    pub fn chance(&mut self, pct_chance: u8) -> bool {
        self.range(0, 100) < pct_chance as u64
    }
}

/// A `ProtocolEvent` paired with the "wall clock" timestamp it should be
/// applied at. Kept alongside the event (rather than derived from it)
/// because several `ProtocolEvent` variants (`Player`, `Scene`) carry no
/// timestamp field of their own — this is the one place scenario code and
/// consumers (tests, `bin/sim.rs`) can rely on a timestamp uniformly.
#[derive(Clone, Debug)]
pub struct SimEvent {
    pub timestamp_ms: u64,
    pub event: ProtocolEvent,
}

/// A named, deterministic preset scenario (issue #40).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// A small party (4 players) fighting a single boss for ~40s — the
    /// baseline "does the overlay look right" scenario.
    NormalParty,
    /// A 20-player raid against a single boss — stresses the row count and
    /// `share_pct` distribution the overlay renders for a full raid.
    Raid20,
    /// A mid-fight disconnect/rejoin: players fight, a `ServerChanged`
    /// event fires (simulating a zone/server hop), and the same players
    /// resume fighting afterward. Exercises the reset path a real
    /// disconnect drives, per issue #40's edge-case open question.
    DisconnectRejoin,
}

const BOSS_UID: i64 = 9000;

/// The real class ids `Class::from` (crates/meter/src/event.rs) maps to a
/// non-`Unknown` class. Not a contiguous `1..=9` range — 6/7/8 are unmapped
/// and fall through to `Class::Unknown`, which has no icon (crates/app/src/
/// icons.rs) — so scenario code must cycle over this list rather than an
/// arithmetic range to guarantee every simulated row gets a real class.
const REAL_CLASS_IDS: [i32; 9] = [1, 2, 3, 4, 5, 9, 11, 12, 13];

impl Scenario {
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::NormalParty => "normal-party",
            Scenario::Raid20 => "raid20",
            Scenario::DisconnectRejoin => "disconnect-rejoin",
        }
    }

    /// Deterministically generates this scenario's full event stream.
    /// Same `(scenario, seed)` always produces the same stream.
    pub fn events(&self, seed: u64) -> Vec<SimEvent> {
        match self {
            Scenario::NormalParty => party_fight(seed, 4, 40_000, BOSS_UID, 1101),
            Scenario::Raid20 => party_fight(seed, 20, 60_000, BOSS_UID, 1102),
            Scenario::DisconnectRejoin => disconnect_rejoin(seed),
        }
    }
}

fn player_info_event(uid: i64, ts: u64) -> SimEvent {
    SimEvent {
        timestamp_ms: ts,
        event: ProtocolEvent::Player(PlayerInfo {
            uid,
            name: Some(format!("Player{uid}")),
            class: Some(crate::event::Class::from(
                REAL_CLASS_IDS[(uid as usize - 1) % REAL_CLASS_IDS.len()],
            )),
            ability_score: Some(1000 + uid as u32 * 10),
            season_strength: Some(1),
        }),
    }
}

fn boss_hp_event(boss_uid: i64, monster_id: u32, curr: u64, max: u64, ts: u64) -> SimEvent {
    SimEvent {
        timestamp_ms: ts,
        event: ProtocolEvent::EnemyHp(crate::event::EnemyHp {
            uid: boss_uid,
            curr_hp: Some(curr),
            max_hp: Some(max),
            monster_id: Some(monster_id),
            timestamp_ms: ts,
        }),
    }
}

fn hit_event(attacker_uid: i64, boss_uid: i64, ts: u64, rng: &mut Rng) -> SimEvent {
    let crit = rng.chance(20);
    let lucky = !crit && rng.chance(10);
    let base = rng.range(50, 500);
    let value = if crit { (base * 2) as i64 } else { base as i64 };
    SimEvent {
        timestamp_ms: ts,
        event: ProtocolEvent::Damage(DamageEvent {
            attacker_uid,
            attacker_kind: EntityKind::Player,
            skill_id: 1,
            value,
            crit,
            lucky,
            hp_lessen: value,
            is_miss: false,
            is_heal: false,
            target_uid: boss_uid,
            target_kind: EntityKind::Monster,
            timestamp_ms: ts,
            // The synthetic sim never kills a player, so every simulated
            // hit is a non-fatal one (issue #49).
            is_dead: false,
        }),
    }
}

/// `party_size` players, each guaranteed at least one hit (so every player
/// shows up as a row), fighting a single boss over `duration_ms`, ticking
/// once per second. `monster_id` distinguishes the boss identity between
/// presets purely for cosmetic/debug purposes.
fn party_fight(
    seed: u64,
    party_size: i64,
    duration_ms: u64,
    boss_uid: i64,
    monster_id: u32,
) -> Vec<SimEvent> {
    let mut rng = Rng::new(seed);
    let mut events = Vec::new();

    for uid in 1..=party_size {
        events.push(player_info_event(uid, 0));
    }
    events.push(SimEvent {
        timestamp_ms: 0,
        event: ProtocolEvent::Scene { level_map_id: 1 },
    });

    let boss_max_hp = 1_000_000u64;
    let tick_ms = 1000u64;
    let ticks = duration_ms / tick_ms;

    for tick in 0..ticks {
        let ts = tick * tick_ms;
        // Every player gets a guaranteed hit on the very first tick so a
        // low-roll RNG can never leave a row missing; subsequent ticks are
        // ~70% chance per player, for a plausible uneven damage spread.
        for uid in 1..=party_size {
            if tick == 0 || rng.chance(70) {
                events.push(hit_event(uid, boss_uid, ts, &mut rng));
            }
        }
        let remaining_pct = 100.0 - (tick as f64 / ticks as f64 * 100.0);
        let curr = (boss_max_hp as f64 * remaining_pct / 100.0).max(0.0) as u64;
        events.push(boss_hp_event(boss_uid, monster_id, curr, boss_max_hp, ts));
    }

    events
}

/// Players fight for `PRE_MS`, a `ServerChanged` event fires at `PRE_MS`
/// (simulating a disconnect), then the same players resume fighting a
/// fresh boss instance for `POST_MS`. Applying this stream through `Meter`
/// must yield exactly one `Some(ResetReason::ServerChange)` at the
/// disconnect point, with the post-reconnect snapshot reflecting only
/// post-reconnect damage (issue #40's edge-case open question).
fn disconnect_rejoin(seed: u64) -> Vec<SimEvent> {
    const PARTY: i64 = 4;
    const PRE_MS: u64 = 10_000;
    const DISCONNECT_MS: u64 = 10_500;
    const POST_START_MS: u64 = 11_000;
    const POST_MS: u64 = 20_000;

    let mut rng = Rng::new(seed);
    let mut events = Vec::new();

    for uid in 1..=PARTY {
        events.push(player_info_event(uid, 0));
    }
    events.push(SimEvent {
        timestamp_ms: 0,
        event: ProtocolEvent::Scene { level_map_id: 1 },
    });

    // Pre-disconnect fight: a handful of ticks of guaranteed damage.
    for tick in 0..(PRE_MS / 1000) {
        let ts = tick * 1000;
        for uid in 1..=PARTY {
            events.push(hit_event(uid, BOSS_UID, ts, &mut rng));
        }
        events.push(boss_hp_event(
            BOSS_UID,
            1,
            900_000 - tick * 10_000,
            1_000_000,
            ts,
        ));
    }

    // Disconnect.
    events.push(SimEvent {
        timestamp_ms: DISCONNECT_MS,
        event: ProtocolEvent::ServerChanged {
            timestamp_ms: DISCONNECT_MS,
        },
    });

    // Rejoin: re-announce players and scene (as a real reconnect would),
    // then resume fighting a fresh boss instance.
    for uid in 1..=PARTY {
        events.push(player_info_event(uid, POST_START_MS));
    }
    events.push(SimEvent {
        timestamp_ms: POST_START_MS,
        event: ProtocolEvent::Scene { level_map_id: 1 },
    });
    for tick in (POST_START_MS / 1000)..(POST_MS / 1000) {
        let ts = tick * 1000;
        for uid in 1..=PARTY {
            events.push(hit_event(uid, BOSS_UID, ts, &mut rng));
        }
        events.push(boss_hp_event(
            BOSS_UID,
            1,
            1_000_000 - (tick - POST_START_MS / 1000) * 10_000,
            1_000_000,
            ts,
        ));
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encounter::Meter;
    use std::collections::BTreeMap;

    /// Runs a scenario's event stream through a fresh `Meter`, returning
    /// the reset reasons observed (in order) and the final snapshot.
    fn run(
        scenario: Scenario,
        seed: u64,
    ) -> (Vec<crate::reset::ResetReason>, crate::stats::Snapshot) {
        let events = scenario.events(seed);
        let mut meter = Meter::new();
        let mut resets = Vec::new();
        let mut last_ts = 0u64;
        for se in &events {
            last_ts = se.timestamp_ms;
            if let Some(reason) = meter.apply(&se.event) {
                resets.push(reason);
            }
        }
        (resets, meter.snapshot(last_ts))
    }

    /// Per-uid (damage, hits) map, used to compare two snapshots
    /// independent of row order (row order is sorted by damage, and ties
    /// aren't given a further deterministic tie-break, so comparing via a
    /// uid-keyed map avoids relying on one).
    fn by_uid(snapshot: &crate::stats::Snapshot) -> BTreeMap<i64, (i64, u64)> {
        snapshot
            .rows
            .iter()
            .map(|r| (r.uid, (r.damage, r.hits)))
            .collect()
    }

    #[test]
    fn same_seed_yields_identical_snapshot() {
        let (resets_a, snap_a) = run(Scenario::NormalParty, 42);
        let (resets_b, snap_b) = run(Scenario::NormalParty, 42);

        assert_eq!(resets_a, resets_b);
        assert_eq!(snap_a.total_damage, snap_b.total_damage);
        assert_eq!(by_uid(&snap_a), by_uid(&snap_b));
    }

    #[test]
    fn different_seeds_yield_different_damage() {
        let (_, snap_a) = run(Scenario::NormalParty, 1);
        let (_, snap_b) = run(Scenario::NormalParty, 2);
        assert_ne!(snap_a.total_damage, snap_b.total_damage);
    }

    #[test]
    fn raid20_yields_20_rows_summing_to_100_share() {
        let (_, snapshot) = run(Scenario::Raid20, 7);
        assert_eq!(snapshot.rows.len(), 20);
        let total_share: f32 = snapshot.rows.iter().map(|r| r.share_pct).sum();
        assert!(
            (total_share - 100.0).abs() < 1.0,
            "total share_pct {total_share} not within 1.0 of 100"
        );
        assert!(snapshot.rows.iter().all(|r| r.hits > 0));
        assert!(
            snapshot
                .rows
                .iter()
                .all(|r| r.class.is_some_and(|c| c != crate::event::Class::Unknown)),
            "every sim-generated row must resolve to a real class, not Unknown"
        );
    }

    #[test]
    fn scenarios_report_distinct_boss_identity() {
        // NormalParty and Raid20 both fight `BOSS_UID`, but pass distinct
        // `monster_id`s (1101/1102) to `party_fight` — regression coverage
        // for the doc comment's claim that `monster_id` distinguishes boss
        // identity between presets (issue #40).
        let (_, normal) = run(Scenario::NormalParty, 1);
        let (_, raid) = run(Scenario::Raid20, 1);
        assert_eq!(normal.encounter.boss_monster_id, Some(1101));
        assert_eq!(raid.encounter.boss_monster_id, Some(1102));
        assert_ne!(
            normal.encounter.boss_monster_id,
            raid.encounter.boss_monster_id
        );
    }

    #[test]
    fn disconnect_rejoin_fires_exactly_one_server_change_reset() {
        let (resets, snapshot) = run(Scenario::DisconnectRejoin, 99);
        assert_eq!(resets, vec![crate::reset::ResetReason::ServerChange]);
        // Every row's damage must come from the post-reconnect fight only:
        // `reset` clears `players`, so pre-disconnect damage cannot survive
        // into the final snapshot.
        assert!(snapshot.total_damage > 0);
        assert_eq!(snapshot.rows.len(), 4);
    }

    #[test]
    fn scenario_name_is_stable() {
        assert_eq!(Scenario::NormalParty.name(), "normal-party");
        assert_eq!(Scenario::Raid20.name(), "raid20");
        assert_eq!(Scenario::DisconnectRejoin.name(), "disconnect-rejoin");
    }

    #[test]
    fn rng_is_deterministic_for_same_seed() {
        let mut a = Rng::new(123);
        let mut b = Rng::new(123);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_zero_seed_does_not_get_stuck() {
        let mut r = Rng::new(0);
        let first = r.next_u64();
        let second = r.next_u64();
        assert_ne!(first, second);
    }
}
