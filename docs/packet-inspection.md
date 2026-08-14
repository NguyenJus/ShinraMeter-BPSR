# Packet inspection: in-game confirmation procedure

Issue #25 built the observation tooling (opt-in diagnostic mode, unrecognized
service/method id logging, unknown attr id logging, and a raw frame dump for
offline replay). That tooling only makes observation *possible* — turning an
observation into a confirmed constant still requires a deliberate procedure
against live game traffic on a Windows box running `shinra-bpsr`. This is
that procedure. It has not been run yet (no Windows box was available while
building slice A); this doc is what to follow the first time someone does.

Do not attach a raw dump to the tracking issue or a PR — dumps contain
player names and other identifying traffic (see `.gitignore`). Extract only
the minimal bytes needed as a synthetic fixture under
`crates/protocol/tests/common/` instead.

## Setup

1. Set `SHINRA_INSPECT=1` before launching `shinra-bpsr.exe` (optionally also
   `SHINRA_INSPECT_DUMP=<path>` to control where the dump lands; otherwise it
   defaults to `%APPDATA%\shinra-bpsr\inspect\dump-<pid>.jsonl`).
2. Play normally. Every unrecognized service uuid and every unknown attr id
   gets logged (`packet-inspect: new ...`) the first time it's seen, with a
   running count/first-seen timestamp/hex prefix summarized again in the log
   when the app exits. The dump file accumulates every Notify-shaped
   fragment observed, decompressed, with timestamps — see the format
   documented in `crates/app/src/dump.rs`.
3. Note wall-clock (or in-app elapsed) timestamps for anything interesting
   you do below, so the corresponding dump records can be found afterward
   (`ts_ms` in each dump line).

## Step 1 — control run: ability score (`FIGHT_POINT`)

Ability score (`attr_id::FIGHT_POINT` / `SyncContainerData.char_base.fight_point`,
already wired up per #24) is the one constant already known to be correct.
Run this first — it's what proves the observe → diff → confirm loop actually
works, before trusting it on anything unknown:

1. Start a capture session, note the character's current ability score in
   the overlay.
2. Swap a piece of gear that changes ability score (or level up), triggering
   a value change server-side.
3. Diff the attr-id log / dump around that moment: the `FIGHT_POINT` id
   should show the old and new values bracketing the gear swap.
4. If this doesn't show up cleanly, something about the capture/dump/log
   path is broken — fix that before trusting results in steps 2–3 below.

## Step 2 — zone change (`#12`)

Blocked on the real service id / `method_id` for `NotifySocialData`
(hypothesized `0x254C89A3`) or an `EnterScene` equivalent, plus the wire
field numbers for `scene_data.line_id`.

1. Start a capture session in one zone.
2. Walk through a zone transition (e.g. take a portal/loading screen) at a
   noted timestamp.
3. Diff which service ids / method ids appear in the dump immediately
   around that timestamp — specifically look for a burst of traffic on a
   service uuid other than `bpsr_protocol::frame::SERVICE_UUID` (logged live
   as "unrecognized service_uuid" if it's genuinely a different service), or
   a previously-unseen `method_id` under the known service if it turns out
   to live there instead.
4. Once a candidate is found, correlate: does it fire *only* around zone
   transitions across multiple repetitions, or does it also fire during
   normal play (ruling it out)?

## Step 3 — season score / imagines (`#15`)

Blocked on an attr id (or proto field) that demonstrably changes when season
level/strength changes in game, and on whether a skill-list attribute
(`AttrSkillLevelIdList`-equivalent) reaches us at all.

1. Start a capture session, note the character's current season
   level/strength (in whatever in-game UI exposes it) and, separately, its
   equipped skill list.
2. Trigger a change: level up the season track, or change strength via
   whatever in-game action does that; separately, equip/unequip an imagine
   skill.
3. Diff the unknown-attr-id log / dump around each moment, the same way as
   the ability-score control run in step 1: look for an attr id (keyed by
   the player's own uid) whose raw value changes exactly when the in-game
   value changes, and stays stable otherwise.
4. For imagines specifically: check whether *any* attr id on the player's
   attr list looks list-shaped (repeated/array-like raw bytes) rather than a
   single scalar — that's the shape `AttrSkillLevelIdList` would take if it
   reaches us at all. If nothing list-shaped shows up, that's itself an
   answer (the data doesn't reach the client this way) worth recording, not
   a failed run.

## Recording a result

Once a constant is confirmed:

1. Extract the minimal bytes that demonstrate it from the dump — a single
   `Attr`/Notify's worth, not the whole session — and add a fixture builder
   to `crates/protocol/tests/common/mod.rs` alongside the existing ones
   (`varint_attr`, `notify`, etc.), plus a regression test that decodes it.
2. Add the constant to `attr_id` / `opcode` in `crates/protocol/src/attrs.rs`
   / `crates/protocol/src/decode.rs`, with a comment noting it was confirmed
   via this procedure (not guessed/copied from another project).
3. Never attach the raw dump itself to the issue or a PR — only the minimal
   synthetic fixture derived from it.
