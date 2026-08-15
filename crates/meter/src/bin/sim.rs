//! Manual/dev viewer for `bpsr_meter::sim` (issue #40): drives a preset
//! scenario through a real `Meter` and prints successive snapshots as
//! plain text, so the encounter/stats pipeline — and, by extension,
//! whatever the overlay renders from a `Snapshot` — can be visually
//! inspected without the game client or a packet capture.
//!
//! ## Why a `bpsr-meter` binary, not an `crates/app` flag
//!
//! `crates/app` (`shinra-bpsr`) pulls in `eframe`/GUI deps and is excluded
//! from `cargo test --workspace` in CI (see
//! `crates/protocol/src/bin/inspect-replay.rs`'s doc comment for the same
//! reasoning applied there). A `src/bin/` binary in `bpsr-meter` needs no
//! new dependency, stays host-runnable/testable in CI, and is the smallest
//! way to get something you can actually run and read — driving the
//! overlay itself from the simulator would mean threading sim state
//! through the app's live-capture wiring for a dev-only path.
//!
//! ## Usage
//!
//! ```text
//! cargo run -p bpsr-meter --bin sim -- [scenario] [seed]
//! ```
//! `scenario` is one of `normal-party` (default), `raid20`,
//! `disconnect-rejoin`. `seed` is a `u64` (default `42`). Same
//! `(scenario, seed)` always prints the same output.

use bpsr_meter::Meter;
use bpsr_meter::sim::Scenario;

const SNAPSHOT_INTERVAL_MS: u64 = 5_000;

fn main() {
    let mut args = std::env::args().skip(1);

    let scenario = match args.next().as_deref() {
        None | Some("normal-party") => Scenario::NormalParty,
        Some("raid20") => Scenario::Raid20,
        Some("disconnect-rejoin") => Scenario::DisconnectRejoin,
        Some(other) => {
            eprintln!("unknown scenario '{other}', falling back to normal-party");
            Scenario::NormalParty
        }
    };
    let seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(42);

    println!("scenario={} seed={seed}\n", scenario.name());

    let events = scenario.events(seed);
    let mut meter = Meter::new();
    let mut next_checkpoint_ms = SNAPSHOT_INTERVAL_MS;

    for se in &events {
        if let Some(reason) = meter.apply(&se.event) {
            println!("--- reset: {reason:?} @ {}ms ---\n", se.timestamp_ms);
        }
        while se.timestamp_ms >= next_checkpoint_ms {
            print_snapshot(&meter, next_checkpoint_ms);
            next_checkpoint_ms += SNAPSHOT_INTERVAL_MS;
        }
    }

    let final_ts = events.last().map(|se| se.timestamp_ms).unwrap_or(0);
    println!("=== final ===");
    print_snapshot(&meter, final_ts);
}

fn print_snapshot(meter: &Meter, now_ms: u64) {
    let snap = meter.snapshot(now_ms);
    println!(
        "t={now_ms}ms  duration={}ms  total_damage={}  total_dps={:.0}  boss={:?}",
        snap.duration_ms, snap.total_damage, snap.total_dps, snap.encounter.boss_monster_id
    );
    for row in &snap.rows {
        println!(
            "  {:<12} {:<12} dmg={:<10} dps={:<8.0} share={:<5.1}% crit={:<5.1}% hits={}",
            row.name,
            row.class.map(|c| c.name()).unwrap_or("?"),
            row.damage,
            row.dps,
            row.share_pct,
            row.crit_pct,
            row.hits
        );
    }
    println!();
}
