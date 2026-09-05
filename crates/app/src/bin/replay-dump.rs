//! `replay-dump`: offline triage over a packet-inspection dump
//! (`SHINRA_INSPECT=1`'s JSONL ring, `crates/app/src/dump.rs`) — replays
//! every record through the *real* decoder and meter, exactly the
//! `decode_notify -> Pipeline::step` wiring
//! `crates/app/tests/common/mod.rs::Rig::feed_notify` uses (reproduced here
//! rather than depended on: the `tests/` directory isn't something a `bin`
//! can pull in), and prints the same `encounter:`-prefixed lifecycle lines
//! the live app logs (`crates/meter/src/encounter.rs`'s `reset_log`,
//! `fight_end_log`, `scene_transition_log`, `boss_transition_log`, and
//! their siblings — all sharing that prefix), each one stamped with the
//! dump-time `ts_ms` of the record that produced it.
//!
//! The point: a user hands over one folder (see `crate::bundle`'s
//! session-bundle export) and an agent can find every bug in the session
//! without the maintainer's help. Comparing this binary's output against
//! the session's own log file (`ShinraMeter-BPSR.log`) shows exactly where
//! the *live* meter's read of the fight diverged from what the captured
//! bytes actually justify — see `docs/packet-inspection.md`'s "Diffing a
//! replay against the live log" section for the procedure.
//!
//! ## Usage
//!
//! ```text
//! cargo run -p ShinraMeter-BPSR --bin replay-dump -- <dump-path> \
//!     [--since <ts_ms>] [--until <ts_ms>] [--snapshot-at-end]
//! ```
//!
//! `<dump-path>` is the *live* (non-numbered) dump file; every numbered ring
//! chunk found next to it is read too, oldest first
//! (`bpsr_protocol::dump_format::load_dump` — same ring-aware read
//! `inspect-replay` uses, see `docs/packet-inspection.md`).
//!
//! `--since`/`--until` (milliseconds, matching the dump's `ts_ms`) narrow
//! the replay to a window, e.g. around a timestamp noted in a bug report.
//! `--snapshot-at-end` additionally prints the final meter snapshot's rows
//! (uid, name if known, total damage, dps) once replay finishes.

use std::path::PathBuf;

use bpsr_app::pipeline::Pipeline;
use bpsr_protocol::dump_format;
use bpsr_protocol::frame::Notify;

/// How often (in dump-time milliseconds) a synthetic tick is fed to the
/// pipeline between records, so idle-timeout-driven state transitions (a
/// fight ending because nothing happened for 9s, `bpsr_meter`'s
/// `FightConfig::default().idle_timeout_ms`) fire during replay the same
/// way they would against a live wall clock — a live session ticks on its
/// own render loop; a replay has no clock but the records' own `ts_ms`, so
/// this stands in for it.
const TICK_INTERVAL_MS: u64 = 100;

/// How far past the last replayed record's `ts_ms` synthetic ticks
/// continue, so a trailing idle timeout gets the chance to fire before
/// `--snapshot-at-end` takes its snapshot — mirrors
/// `crates/app/tests/replay_dump.rs`'s `POST_FIXTURE_TICK_MARGIN_MS`
/// (comfortably past the meter's 9s default idle timeout).
const TAIL_MARGIN_MS: u64 = 20_000;

thread_local! {
    /// The current record's `ts_ms`, set immediately before every
    /// `Pipeline::step`/`tick` call — read by [`TsPrefixLogger`] to stamp
    /// each `encounter:` line it prints. Thread-local rather than a plain
    /// global: nothing about `TsPrefixLogger::log` requires
    /// single-threadedness, but `main` below only ever runs on one thread,
    /// so a thread-local costs nothing extra and needs no synchronization.
    static CURRENT_TS_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Installed as the process-wide `log::Log` in place of `env_logger` (this
/// binary is not the overlay; `bpsr_app::logging::init` is never called
/// here). Prints only the `encounter:`-prefixed lifecycle lines
/// `crates/meter/src/encounter.rs`'s builders emit — every other
/// `log::info!`/`log::debug!`/`log::warn!` call anywhere in the
/// decode/meter/pipeline stack is deliberately dropped, since this
/// binary's whole job is the lifecycle narrative, not a general log
/// viewer.
struct TsPrefixLogger;

impl log::Log for TsPrefixLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if !message.starts_with("encounter:") {
            return;
        }
        let ts_ms = CURRENT_TS_MS.with(std::cell::Cell::get);
        println!("[ts_ms={ts_ms}] {message}");
    }

    fn flush(&self) {}
}

static LOGGER: TsPrefixLogger = TsPrefixLogger;

/// Parsed command-line arguments — see the module doc comment for the
/// usage line. Split out from `main` as a pure function so argument
/// parsing is unit-tested without a process to run.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    dump_path: PathBuf,
    since: Option<u64>,
    until: Option<u64>,
    snapshot_at_end: bool,
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let dump_path = args.next().ok_or_else(|| {
        "usage: replay-dump <dump-path> [--since <ts_ms>] [--until <ts_ms>] [--snapshot-at-end]"
            .to_string()
    })?;
    let mut since = None;
    let mut until = None;
    let mut snapshot_at_end = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since" => {
                let value = args.next().ok_or("--since needs a value")?;
                since = Some(
                    value
                        .parse::<u64>()
                        .map_err(|err| format!("--since: {err}"))?,
                );
            }
            "--until" => {
                let value = args.next().ok_or("--until needs a value")?;
                until = Some(
                    value
                        .parse::<u64>()
                        .map_err(|err| format!("--until: {err}"))?,
                );
            }
            "--snapshot-at-end" => snapshot_at_end = true,
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(Args {
        dump_path: PathBuf::from(dump_path),
        since,
        until,
        snapshot_at_end,
    })
}

/// `true` when `ts_ms` falls inside the `[since, until]` window (either or
/// both bounds `None` meaning unbounded on that side) — pure, so the
/// filtering logic `main`'s replay loop applies is unit-tested without a
/// dump file.
fn in_window(ts_ms: u64, since: Option<u64>, until: Option<u64>) -> bool {
    since.is_none_or(|since| ts_ms >= since) && until.is_none_or(|until| ts_ms <= until)
}

fn main() {
    log::set_logger(&LOGGER).expect("install the replay-dump logger");
    log::set_max_level(log::LevelFilter::Info);

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    let mut records = match dump_format::load_dump(&args.dump_path) {
        Ok(records) => records,
        Err(err) => {
            eprintln!("failed to read dump at {}: {err}", args.dump_path.display());
            std::process::exit(1);
        }
    };
    // `load_dump` already reads ring chunks oldest-first followed by the
    // live file, but a defensive sort costs nothing and guards against a
    // clock that ran backwards across a rotation.
    records.sort_by_key(|r| r.ts_ms);

    let mut pipeline = Pipeline::new();
    let mut next_tick: Option<u64> = None;
    let mut last_ts = 0u64;
    let mut fed_any = false;
    // Cross-packet entity identity (issue #335), kept for the whole replay
    // the same way a live `Decoder` keeps it for the whole session.
    let mut entities = bpsr_protocol::EntityTable::new();

    for record in &records {
        if !in_window(record.ts_ms, args.since, args.until) {
            continue;
        }

        // Catch synthetic ticks up to this record's timestamp before
        // stepping it in, so an idle-timeout-driven transition between the
        // previous record and this one fires at the right dump-time
        // instant rather than only once per record.
        if next_tick.is_none() {
            next_tick = Some(record.ts_ms);
        }
        while let Some(t) = next_tick {
            if t > record.ts_ms {
                break;
            }
            CURRENT_TS_MS.with(|cell| cell.set(t));
            pipeline.tick(t);
            next_tick = Some(t + TICK_INTERVAL_MS);
        }

        // A record the original capture couldn't decompress carries raw
        // compressed bytes, not protobuf — decode_notify would just fail
        // to parse it; skip it like a real decoder would (see
        // `DumpRecord::payload_decoded`'s doc comment).
        if !record.payload_decoded {
            continue;
        }

        let notify = Notify {
            service_uuid: record.service_uuid,
            method_id: record.method_id,
            payload: record.payload.clone(),
        };
        let mut events = Vec::new();
        bpsr_protocol::decode::decode_notify(
            &notify,
            record.ts_ms,
            &mut events,
            None,
            &mut entities,
        );

        CURRENT_TS_MS.with(|cell| cell.set(record.ts_ms));
        for ev in events {
            pipeline.step(ev, record.ts_ms);
        }
        last_ts = record.ts_ms;
        fed_any = true;
    }

    if !fed_any {
        eprintln!("no records in the requested window; nothing replayed");
        std::process::exit(1);
    }

    // A margin of extra synthetic ticks past the last replayed record, so a
    // trailing idle-timeout fight end gets the chance to fire — see
    // `TAIL_MARGIN_MS`.
    let tail_end = last_ts + TAIL_MARGIN_MS;
    let mut t = next_tick.unwrap_or(last_ts).max(last_ts + TICK_INTERVAL_MS);
    while t <= tail_end {
        CURRENT_TS_MS.with(|cell| cell.set(t));
        pipeline.tick(t);
        t += TICK_INTERVAL_MS;
    }

    if args.snapshot_at_end {
        CURRENT_TS_MS.with(|cell| cell.set(tail_end));
        let snapshot = pipeline.snapshot(tail_end);
        println!("--- snapshot at ts_ms={tail_end} ---");
        let mut rows: Vec<_> = snapshot.rows.iter().collect();
        rows.sort_by(|a, b| b.damage.cmp(&a.damage).then(a.uid.cmp(&b.uid)));
        for row in rows {
            let name = if row.name.is_empty() {
                "-"
            } else {
                row.name.as_str()
            };
            println!(
                "uid={} name={} damage={} dps={:.2}",
                row.uid, name, row.damage, row.dps
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_args -------------------------------------------------------

    #[test]
    fn parse_args_requires_a_dump_path() {
        let err = parse_args(std::iter::empty()).unwrap_err();
        assert!(err.contains("usage:"));
    }

    #[test]
    fn parse_args_accepts_a_bare_path() {
        let args = parse_args(["dump.jsonl".to_string()].into_iter()).unwrap();
        assert_eq!(
            args,
            Args {
                dump_path: PathBuf::from("dump.jsonl"),
                since: None,
                until: None,
                snapshot_at_end: false,
            }
        );
    }

    #[test]
    fn parse_args_reads_since_until_and_snapshot_at_end() {
        let args = parse_args(
            [
                "dump.jsonl",
                "--since",
                "100",
                "--until",
                "200",
                "--snapshot-at-end",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(
            args,
            Args {
                dump_path: PathBuf::from("dump.jsonl"),
                since: Some(100),
                until: Some(200),
                snapshot_at_end: true,
            }
        );
    }

    #[test]
    fn parse_args_rejects_a_non_numeric_since() {
        let err = parse_args(
            ["dump.jsonl", "--since", "not-a-number"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap_err();
        assert!(err.starts_with("--since:"));
    }

    #[test]
    fn parse_args_rejects_an_unrecognized_flag() {
        let err =
            parse_args(["dump.jsonl", "--bogus"].into_iter().map(str::to_string)).unwrap_err();
        assert!(err.contains("--bogus"));
    }

    // -- in_window ----------------------------------------------------------

    #[test]
    fn in_window_is_unbounded_with_no_since_or_until() {
        assert!(in_window(0, None, None));
        assert!(in_window(u64::MAX, None, None));
    }

    #[test]
    fn in_window_respects_since_and_until_inclusively() {
        assert!(!in_window(99, Some(100), None));
        assert!(in_window(100, Some(100), None));
        assert!(in_window(200, None, Some(200)));
        assert!(!in_window(201, None, Some(200)));
        assert!(in_window(150, Some(100), Some(200)));
    }
}
