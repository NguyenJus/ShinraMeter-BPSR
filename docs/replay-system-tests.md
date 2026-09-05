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
- `crates/app/tests/replay_entities.rs` — entity identity (issue #335): two
  distinct entities wearing the same display uid (`uuid >> 16`) — a recycled
  uid, or a shadow/mirror copy — must keep separate rows and totals rather
  than blending into one.
- `crates/app/tests/replay_dump.rs` — replays a real, sanitized packet
  capture (not a hand-built scenario) through the same pipeline. See
  "Replaying a real capture" below.
- `crates/app/tests/replay_scenarios.rs` — issue #342's first batch of
  scenarios not covered above: starting mid-instance (no `EnterScene` before
  damage), a server change landing in a *new* dungeon mid-pull, a party wipe
  followed by a re-pull, a world boss holding a pull open through a long
  lull, and a curated multi-phase boss transition.

Each synthetic scenario is built from the `Scenario`/`Step` DSL in
`crates/test-support/src/scenario.rs` (byte payloads via
`crates/test-support/src/wire.rs`, delivered in a chosen order/segmentation)
and run through `crates/app/tests/common/mod.rs`'s `Rig`, which owns the
reassembler, decoder, and pipeline and records a `Capture` (snapshot +
fight-state + resets) at each `Step::Capture` point.

`Rig` has a second entry point, `Rig::feed_notify(method_id, payload,
ts_ms)`, for input that is already a decoded Notify body (post-zstd,
post-frame-split) rather than raw TCP bytes — exactly the shape of one
`bpsr_protocol::dump_format::DumpRecord`. It goes straight to
`decode_notify -> Pipeline::step`, skipping `TcpReassembler`/
`Decoder::push_stream` entirely, and shares only the pipeline/reset state
with `run`/`run_bytes` — it cannot perturb the byte-level scenarios above.

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

## Replaying a real capture

`crates/app/tests/replay_dump.rs` replays
`crates/app/tests/fixtures/dump-2976-boss-fight.jsonl.zst` — a ~209s window
of one real boss fight (monster id 1152, "Kartgriff", in scene id 8,
"Asterleeds") from a real, sanitized packet capture — through
`Rig::feed_notify`. Unlike the synthetic scenarios above, this test proves
the pipeline against actual wire traffic, not just our own understanding of
the protocol encoded into hand-built bytes.

### Regenerating the fixture

The fixture is built from a raw `SHINRA_INSPECT=1` JSONL dump (see
`crates/app/src/dump.rs`'s doc comment for that on-disk format) with
`crates/protocol/src/bin/sanitize-dump`:

```
cargo run -p bpsr-protocol --bin sanitize-dump -- \
    dump-2976.jsonl \
    --out crates/app/tests/fixtures/dump-2976-boss-fight.jsonl.zst \
    --since 1787022118000 --until 1787022327000
```

See "Known limitation: identity attrs are sent once, not on every delta"
below for why `--since` needs real margin before the first damage event,
not just enough to cover it.

`--since`/`--until` are the raw dump's own `ts_ms` (milliseconds since the
Unix epoch); the sanitizer rebases the output to start near zero so the
fixture carries no wall-clock information about when it was captured.

### Safety model: whitelist by re-encode

Each record's payload is decoded with `pb.rs` — a hand-written *partial*
protobuf schema — and re-encoded. Any field `pb.rs` doesn't model is
silently dropped by that round-trip, and any opcode `pb.rs` doesn't model at
all is dropped from the output entirely (`sanitize::is_modeled`). That drop
is the safety property: nothing exits the tool that isn't explicitly
accounted for, so an unmodeled field (e.g. the 36-char session GUIDs riding
`EnterScene`'s unmodeled fields, or an unrelated player's raw name in a
message shape we don't otherwise care about) can't leak just because it was
sitting next to something we do model.

Player-identifying uids are consistently remapped and names replaced with
stable `PlayerNNNNN` placeholders. The tool refuses to write any output
unless both self-checks pass:

1. **Fingerprint equality** — replaying the original window's records and
   the sanitized records separately through
   `decode_notify -> Meter::apply -> Meter::snapshot` must produce
   identical fingerprints (duration, total damage/DPS, every row's stats,
   and the full `EncounterInfo`) for every fight-end snapshot plus the
   final snapshot. This is what would have caught the bare-uid trap below
   before it ever became a bug.
2. **No residual strings** — every field that can carry text in any of the
   five modeled message shapes must decode to the `PlayerNNNNN` placeholder
   pattern (or be empty).

A failure on either check aborts before any output is written.

### The bare-uid vs packed-uuid trap

Most identity fields on the wire are a *packed* `uuid = (uid << 16) |
kind_bits` — remapping preserves the low 16 bits (entity kind) and only
remaps the uid half. But `CharSerialize.char_id` and `CharBaseInfo.char_id`
are *bare* uids, not packed uuids. Applying the packed-uuid rule to one of
them silently corrupts the low 16 bits of an otherwise-fine uid, which
manifests as the local player's `class`/`ability_score` quietly going
missing — while every damage number stays byte-identical, since damage
attribution never touches `char_id`. This bit an earlier attempt at this
tool; it's exactly the class of bug fingerprint equality (self-check 1)
exists to catch, since a byte-identical damage fingerprint gives no signal
that class/ability_score broke.

### Known limitation: the fixture only sees what the current decoder sees

The sanitizer decodes with the *current* `pb.rs` schema before re-encoding,
so a fixture built today reflects today's decoder's understanding of the
protocol. If `pb.rs` is later extended to model a new field or opcode, this
fixture won't retroactively gain that data — it has to be regenerated from
the original raw dump. Raw dumps live only on the developer's machine
(`%APPDATA%\ShinraMeter-BPSR\inspect\dump-*.jsonl`, gitignored, never
committed) — archive one off-repo if you want the option to extend a
fixture later, because once it rotates out or the machine is wiped, that
window's fixture can never be regenerated with a richer schema.

### Known limitation: identity attrs are sent once, not on every delta

An entity's identifying attrs (`MONSTER_ID`, `PROFESSION_ID`, etc.) are only
broadcast on its first `SyncNearEntities` "appear" packet; every later
`SyncNearDeltaInfo`/`SyncToMeDeltaInfo` update for that entity carries only
what changed (usually just HP). If `--since` is chosen at or after the
first damage event, and the boss (or a player) had already been in view for
some time before the pull started, that one identity-carrying packet can
fall *before* the window and never appear in the fixture at all — even
though the entity is damaged throughout the whole window. When building a
new fixture, err on the side of starting `--since` earlier than the first
damage timestamp, not exactly at it, to leave margin for identity packets
that fired while the party was still approaching.

**This is not hypothetical — it is what made the first version of
`replay_dump_boss_fight` fail.** Kartgriff's (1152) one and only
`MONSTER_ID` sighting in the raw `dump-2976.jsonl` is at
`ts=1787022133362`. An earlier fixture window used
`--since=1787022142877`, about 9.5s *after* that sighting, so the meter
tracked the boss's HP and damage correctly but never learned its table id
and `encounter.boss_monster_id` resolved to `None`. The current fixture
uses `--since=1787022118000`, about 15.4s *before* the sighting — chosen
by scanning the raw dump for every `SYNC_NEAR_ENTITIES` appear carrying a
`MONSTER_ID`/`NAME`/`PROFESSION_ID` attr near the pull, not by guessing a
round number. That margin also happens to land in a real ~45s quiet gap
between an earlier, unrelated trash pull by the same party (which ends at
`ts=1787022112670`) and this fight's first damage — worth checking for
when picking `--since` on any future regeneration, since starting the
window mid-fight on an unrelated earlier pull would splice two encounters
into one fixture.

### This fixture has one identified player; the other four have no name or class data

Only one `SyncContainerData` (the packet that carries a player's real name,
class, and ability_score) exists anywhere in the raw dump, at
`ts=1787022325888` — the game only broadcasts it for the local player, not
party members. The current window's `--until=1787022327000` was chosen to
include it (along with the raw dump's only `EnterScene` record, at the same
timestamp, which is how `scene_id`/`scene_name` resolve for real rather
than being absent). That gives one row real identity: `Player100005`,
`class = Some(Class::ShieldKnight)`, `ability_score = Some(62186)`. The
other four rows still resolve to `Player <uid>` with `class = None` and
`ability_score = None` — that's a real coverage gap, not a bug in the
sanitizer or the pipeline: this fixture proves the identity-attribution
path for exactly one row, not all five. Do not write assertions on
names/class/ability_score for the other four rows; they are structurally
absent. A future fixture built from a window that includes party members'
own `SyncContainerData` packets (if the game ever sends them) would be
needed to cover that path for everyone.

## Limitation: the synthetic scenarios are not a substitute for live verification

Every byte in `replay_pull.rs`/`replay_lifecycle.rs`/`replay_scenarios.rs`'s scenarios is built
from our own understanding of the protocol (the same builders used in
`crates/protocol`'s unit tests), not captured from a real client. That means
this suite proves the pipeline behaves *consistently* with what we believe
the protocol looks like — it
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
