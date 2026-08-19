# Replay system tests

`crates/app/tests/` runs the real ingest pipeline — TCP reassembly, the
protocol decoder, and the meter's fight-state machine — end to end over
hand-built byte scenarios, then compares the resulting snapshot against a
checked-in golden file. This is the app crate's system-test layer: unlike
the unit tests in `crates/protocol` and `crates/meter`, these exercise the
same `bpsr_app::pipeline::Pipeline` the real binary drives, wired the same
way `main.rs` wires it.

## What's covered

- `crates/app/tests/replay_pull.rs` — a fight pull with multiple players,
  a boss-kill title change, and a pull whose bytes arrive split across
  multiple TCP segments (`tcp_segmented_pull`), exercising reassembly
  ordering.
- `crates/app/tests/replay_lifecycle.rs` — fight-state transitions: a boss
  HP rollback that should auto-reset the fight, a server-change reset, an
  idle-timeout freeze, and pet damage credited to its owner.

Each scenario is built from the `Scenario`/`Step` DSL in
`crates/test-support/src/scenario.rs` (byte payloads via
`crates/test-support/src/wire.rs`, delivered in a chosen order/segmentation)
and run through `crates/app/tests/common/mod.rs`'s `Rig`, which owns the
reassembler, decoder, and pipeline and records a `Capture` (snapshot +
fight-state + resets) at each `Step::Capture` point.

## Running them

```
cargo test -p ShinraMeter-BPSR --tests
```

or just `cargo test --workspace` to run them alongside everything else.

## Regenerating goldens

Each capture is rendered to plain text and diffed against
`crates/app/tests/goldens/<label>.txt`. If a scenario intentionally
changes (not a bug — an actual behavior change), regenerate:

```
SHINRA_UPDATE_GOLDENS=1 cargo test -p ShinraMeter-BPSR --tests
git diff crates/app/tests/goldens
```

Review the diff carefully, then re-run the test **without** the env var to
confirm it now passes. Never hand-edit a golden file.

## Limitation: these are synthetic, not a substitute for live verification

Every byte in these scenarios is built from our own understanding of the
protocol (the same builders used in `crates/protocol`'s unit tests), not
captured from a real client. That means this suite proves the pipeline
behaves *consistently* with what we believe the protocol looks like — it
catches regressions when someone changes the decoder, the reassembler, or
the fight-state machine in a way that breaks an existing scenario. It does
**not** prove our understanding of the protocol is correct against live
game traffic, and it does **not** replace a live smoke test after the game
patches — a server-side protocol change can silently invalidate every
scenario here at once (they'd all still pass, because they encode the old
assumptions). See `docs/packet-inspection.md` for the procedure that
validates against a real client.

This suite also does not touch: UI rendering (the overlay window itself),
WinDivert/UAC (packet capture and elevation), or window composition
(`SHINRA_NO_COMPOSITION` and friends) — all of that remains manual-only,
Windows-only verification.
