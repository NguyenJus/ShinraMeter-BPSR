# Packet inspection: in-game confirmation procedure

Issue #25 built the observation tooling (diagnostic mode — opt-in since issue
#122 via `SHINRA_INSPECT=1` — unrecognized service/method id logging, unknown
attr id logging, and a raw frame dump for offline replay).
That tooling only makes observation *possible* — turning an
observation into a confirmed constant still requires a deliberate procedure
against live game traffic on a Windows box running `ShinraMeter-BPSR`. This is
that procedure. It has not been run yet (no Windows box was available while
building slice A); this doc is what to follow the first time someone does.

Do not attach a raw dump to the tracking issue or a PR — dumps contain
player names and other identifying traffic (see `.gitignore`). Extract only
the minimal bytes needed as a synthetic fixture under
`crates/protocol/tests/common/` instead.

## Setup

1. Set `SHINRA_INSPECT=1` before launching `ShinraMeter-BPSR.exe` (optionally also
   `SHINRA_INSPECT_DUMP=<path>` to control where the dump lands; otherwise it
   defaults to `%APPDATA%\ShinraMeter-BPSR\inspect\dump-<session_id>.jsonl`,
   where `<session_id>` is `<pid>-<unix start seconds>`, issue #322 — not a
   bare pid, which gets reused across runs and can't tell two sessions'
   dumps apart). The startup log banner prints the same session id
   (`ShinraMeter-BPSR vX.Y.Z starting (pid ..., session <session_id>, ...`),
   so a dump on disk can always be matched back to the log of the session
   that produced it.
2. Play normally. Every unrecognized service uuid and every unknown attr id
   — on enemy entities as well as player ones — gets logged
   (`packet-inspect: new ...`) the first time it's seen, with a running
   count/first-seen timestamp/hex prefix summarized again in the log when the
   app exits. The dump file accumulates every Notify-shaped fragment
   observed, decompressed, with timestamps — see the format documented in
   `crates/app/src/dump.rs`. A fragment whose payload *fails* to decompress
   (corrupt, or a codec we don't speak) is dumped too, as its raw bytes with
   `"payload_decoded":false` — that traffic is exactly what this mode exists
   to surface, so it is never dropped on the way to the dump.
3. Check the shutdown log for `inspect dump is INCOMPLETE`. The dump channel
   is bounded and drops records rather than back-pressuring packet capture,
   so a stalled disk can thin a dump; that line (absent when nothing was
   dropped) says how many records were lost, and therefore how much to trust
   a count in the replay report below. Separately, watch for `inspect dump
   rotated` and `inspect dump ring exceeded its ... byte budget; deleted
   oldest chunk ...` lines (issue #322) — a rotation is now logged every
   time it happens (bytes written plus the rotated-out chunk's `ts_ms`
   range), and a ring-budget eviction names exactly which chunk it deleted,
   so a long session no longer loses data with zero log signal.
4. Note wall-clock (or in-app elapsed) timestamps for anything interesting
   you do below, so the corresponding dump records can be found afterward
   (`ts_ms` in each dump line).

## Offline replay: `inspect-replay`

Slice B added a small binary, `crates/protocol/src/bin/inspect-replay.rs`, that
reads a dump file and re-runs the decoder over it *offline* — no live game or
Windows box needed, so this is the tool to reach for while doing the diffing
in steps 1-3 below. It rebuilds the same histograms a live run would have
logged: every service id and method id observed (count, first/last-seen
`ts_ms`, and whether it's one we currently decode), plus every attr id
observed on any entity's attr list — split into a "known" section (ids
`attrs::attr_id` has a constant for, e.g. `FIGHT_POINT`) and an
"unrecognized" section (no constant), each with a count, first/last-seen
`ts_ms`, and a sample uid/raw bytes. The known section is what step 1's
control run diffs — `FIGHT_POINT`'s raw value should visibly change across
the gear swap; the unrecognized section is what steps 2-3 diff for a
brand-new candidate id. Unrecognized service ids, undecoded method ids, and
unrecognized attr ids are called out distinctly from known ones in every
section — that distinction is the entire point of the tool. A final section
totals the records whose payload the capture couldn't decompress: they still
count towards the service/method histograms (a foreign codec is itself a
finding) but contribute no attr ids, so a nonzero total there explains a
thinner-than-expected attr section.

Each dump *chunk* caps at 50 MiB and rotates into a numbered ring —
`dump-<session_id>.jsonl`, `.1`, `.2`, ... (see `crates/app/src/dump.rs` and
the README's "Packet inspection dumps" section) — up to a **512 MiB total
ring budget** by default, overridable with `SHINRA_INSPECT_MAX_BYTES=<bytes>`
(issue #322). Once the ring exceeds its budget, the oldest (highest-numbered)
chunk is deleted, logged at info with its path and size. This replaced the
old fixed 2-chunk / 100 MiB scheme: a raid captured for issue #285 emitted
roughly **2.5 MB/min**, so a 90+ minute raid runs on the order of 240 MB, and
the old cap silently lost the front ~70% of longer sessions with no log
signal that anything had been discarded. 512 MiB holds a bit over 3 hours at
that rate — comfortably more than one raid; size `SHINRA_INSPECT_MAX_BYTES`
up from real numbers if your sessions run longer. If the *entire* ring
(every numbered chunk plus the live file) comes back at budget, treat the
window as truncated at the front — check the log for `inspect dump ring
exceeded its ... byte budget; deleted oldest chunk ...` lines to see exactly
what got dropped and when (approximately, from the surrounding rotation
lines' `ts_ms` ranges). `inspect-replay` handles the ring itself via
`bpsr_protocol::dump_format::load_dump`: every numbered chunk found next to
the file you pass it is read first, oldest (highest-numbered) to newest, so
records stay in chronological order, and it prints a `note:` line per chunk
saying how many records it contributed. You only need to point it at the
live (non-numbered) path.

Run it against a dump:

```
cargo run -p bpsr-protocol --bin inspect-replay -- path/to/dump-<session_id>.jsonl
```

Narrow it to a window around a noted timestamp with `--since`/`--until`
(milliseconds, matching the dump's `ts_ms`), e.g. to look at just the minute
around a zone transition noted at `ts_ms=1_699_999_000_000`:

```
cargo run -p bpsr-protocol --bin inspect-replay -- dump.jsonl \
  --since 1699999000000 --until 1699999060000
```

Run it twice with different windows (before/after the event) and compare the
two reports by eye — a service/method/attr id that only appears in the
"after" window is the candidate. `cargo test -p bpsr-protocol` covers the
reader itself against synthetic fixtures; it never needs a real dump to pass.

## Offline replay: `replay-dump` (the live meter's lifecycle narrative)

`inspect-replay` above rebuilds *histograms* — what ids showed up, how
often. It says nothing about what the meter itself *concluded*: which boss
it decided was being fought, when it decided a fight ended, which scene it
thinks the party is in. `crates/app/src/bin/replay-dump.rs` fills that gap:
it drives the exact same decoder and `bpsr_meter` pipeline the live overlay
runs (`bpsr_app::pipeline::Pipeline`, the same `decode_notify ->
Pipeline::step` wiring `crates/app/tests/common/mod.rs`'s `Rig::feed_notify`
uses) over a dump, offline, and prints the same `encounter:`-prefixed
lifecycle lines the live app logs — `reset_log`, `fight_end_log`,
`scene_transition_log`, `boss_transition_log`, and their siblings in
`crates/meter/src/encounter.rs` — each stamped with the dump-time `ts_ms` of
the record that produced it. It lives in the app crate rather than
alongside `inspect-replay` in `bpsr-protocol` specifically because it needs
`Pipeline`/`Meter`, which only the app crate wires together.

A synthetic tick every 100ms of dump time stands in for the live overlay's
own render-loop tick, so idle-timeout-driven transitions (a fight ending
because nothing happened for 9s) fire during replay the same way they would
live, including a trailing margin of ticks past the last record so a fight
still active at the end of the window gets the chance to end before replay
finishes.

```
cargo run -p ShinraMeter-BPSR --bin replay-dump -- path/to/dump-<session_id>.jsonl
```

Same `--since`/`--until` window as `inspect-replay` (milliseconds, matching
`ts_ms`); add `--snapshot-at-end` to also print the final meter snapshot's
rows (uid, name if known, total damage, dps) once replay finishes:

```
cargo run -p ShinraMeter-BPSR --bin replay-dump -- dump.jsonl \
  --since 1699999000000 --until 1699999060000 --snapshot-at-end
```

### Diffing a replay against the live log

This is the tool an agent reaches for when a session bundle (the "Export
session bundle" header-menu item, `crates/app/src/bundle.rs`) shows up with
no maintainer around to explain it — the bundle's `manifest.json` says
whether `SHINRA_INSPECT` was on and, if so, exactly which dump file to point
this binary at:

1. Run `replay-dump` against the bundle's dump file (all of its numbered
   ring chunks are read automatically, same as `inspect-replay`) and save
   its stdout.
2. Filter the bundle's own log file (`ShinraMeter-BPSR.log`/`.log.1`) down
   to the same `encounter:`-prefixed lines the live app logged, e.g.
   `grep 'encounter:'`.
3. Diff the two line-for-line. Both are the same lifecycle narrative,
   independently produced — the log by the live process reading real wire
   traffic in real time, the replay by re-running the *exact same bytes*
   offline with no game session behind them. They should read identically:
   the same boss recognized at the same moment, the same fight ends, the
   same scene transitions, each within a tick or two of `ts_ms` (the
   replay's synthetic 100ms tick versus the live app's own render cadence
   accounts for small timing drift; a difference of *seconds*, or a
   transition present in one and missing from the other entirely, is not
   drift).
4. A divergence — the log says a fight ended, the replay's boss target
   never even changed against those bytes; the log names a scene the
   replay resolves differently; a "dropped %d record(s)" summary in the
   log's shutdown lines (see `manifest.json`'s `dropped_records`) that
   would explain a gap in the replay but not in the live run — is the
   symptom. What the log says the live meter decided and what these exact
   bytes justify have parted ways, and the gap between them is where the
   bug lives: either the decoder/meter logic itself, or (if
   `dropped_records` is nonzero) a dump that is missing the very records
   that would explain the live behavior, in which case the log alone —
   not the incomplete replay — is the more trustworthy account of what
   happened.

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

Season score is **done**. Imagine skill classification (which two of a
player's skills are their equipped Imagines) is **not implemented** —
recorded below so the investigation isn't lost. Separately, issue #37 fixed a
related but distinct gap: recognizing when a profession id names an Imagine
*transform* rather than a real class, so it doesn't clobber the transformed
player's row — also recorded below.

### Season score: done, via reference-derived ids

No packet capture was available while building this, so `attr_id::SEASON_LEVEL`
(`0x2756`) and `attr_id::SEASON_STRENGTH` (`0x2CB0`) were reimplemented from
BPSR-ZDPS's `EnumEAttrType.cs` (`AttrSeasonLevel = 10070`,
`AttrSeasonStrength = 11440`) rather than confirmed against captured traffic —
the one exception the repo owner sanctioned to this doc's normal
confirm-before-committing rule (see "Recording a result" below). Both decode
as plain varints on the same per-entity `Attr` list `FIGHT_POINT` already
uses, so they're expected to reach us for any nearby entity, not just the
local player.

Re-verifying against a real capture, whenever one becomes available, is the
same procedure as step 1's ability-score control run: note the character's
current season level/strength in whatever in-game UI exposes it, trigger a
change (level up the season track, or change strength via whatever in-game
action does that), and diff the attr-id log/dump around that moment —
`SEASON_LEVEL`/`SEASON_STRENGTH` should show the old and new values
bracketing the change. Short of a capture, the simplest live check is just
whether the two columns (off by default; enable them from the settings menu)
populate at all in-game.

### Imagines: not implemented

Blocked on classification, not on data availability. The reference carries a
player's full skill list as attr `0x74` (116, `AttrSkillLevelIdList`), a
repeated length-delimited message of `{ skillId = 1, currentLevel = 2,
remodelLevel = 3 }`, on the same generic per-entity attr channel — so, like
season score, it would be available for other players too, not just the
local one. Imagine damage is attributed to the owning player via
`SyncDamageInfo.TopSummonerId` (field 21, nonzero → credit that uuid instead
of the summon's) — that part is unremarkable and already mirrors how pet
damage is attributed here (`on_aoi_sync_delta`'s `top_summoner_id` handling).

What's missing is a way to tell which two of a player's skills are their
equipped Imagines. The reference answers this with a static `SkillId ->
slot/name/icon` table (checking whether `SlotPositionId` contains 7 or 8),
built from a 10.8 MB `Data/SkillTable.json` extracted from the game client.
There is no id range, bit pattern, or other protocol-native substitute for
that table — even falling back to just the icon path needs the same lookup.
That table has no stated license or provenance in the reference repo (its
MIT license covers only the C# source, not the extracted game data), and its
own slot-7/8 heuristic has ~15 known false positives out of 125 matching
rows. Redistributing it is therefore out of scope for this GPL-3.0 project.
If a licensable or protocol-native classification source ever turns up,
`AttrSkillLevelIdList` decoding itself should be straightforward — it's the
table, not the wire format, that's blocking this.

### Imagine profession ids: done, via reference-derived ids

Separately from the skill-classification gap above, issue #37 found that
`cur_profession_id` / `ATTR_PROFESSION_ID` itself reads one of four ids while
an Imagine transform is active (Dorothy, Dark Spirit Dance, Lucy, Natsu) — not
a real player class — and `Class::from` was silently mapping each of them to
`Class::Unknown`, clobbering the transformed player's real class for the
transform's duration.

No packet capture was available while fixing this, so `pb::IMAGINE_PROFESSION_IDS`
(`[8, 10, 14, 15]`) was reimplemented from BPSR-ZDPS's `EProfessionId`
(`Dorothy = 8`, `DarkSpiritDance = 10`, `Lucy = 14`, `Natsu = 15`) rather than
confirmed against captured traffic — the same kind of exception to this doc's
normal confirm-before-committing rule that `attr_id::SEASON_LEVEL` /
`attr_id::SEASON_STRENGTH` document above, sanctioned by the repo owner on the
same basis. Decoders resolve a raw id via `pb::class_of_profession_id`, which
returns `None` for these four ids (rather than `Some(Class::Unknown)`) so the
existing "`Some` overwrites, `None` preserves" merge rule already used
elsewhere leaves the player's real class untouched.

Re-verifying against a real capture, whenever one becomes available, is the
same procedure as `SEASON_LEVEL`/`SEASON_STRENGTH` above: trigger each
Imagine transform in-game, note it against the attr-id log/dump, and confirm
`cur_profession_id`/`ATTR_PROFESSION_ID` reads the expected id for its
duration and reverts afterward.

## Step 4 — party roster (`GrpcTeamNtf`, #146)

Issue #146 adopted a **second** protobuf service riding the same
already-adopted TCP connection: `GrpcTeamNtf`
(`bpsr_protocol::frame::TEAM_NTF_SERVICE_UUID`, `0x0000_0000_399F_CA69`).
Its `NotifyJoinTeam` method (`0x3` on this service —
`bpsr_protocol::decode::team_opcode::NOTIFY_JOIN_TEAM`, distinct from
`ENTER_SCENE`'s `0x3` on the main service; `decode_notify` dispatches on the
`(service_uuid, method_id)` pair specifically so the two don't collide)
carries a bulk party-roster push: name, class, and ability score for every
member, decoded into a `ProtocolEvent::Player` per member the same way
`SyncContainerData` and the AOI attr paths already are.

**Field tags are not reference-derived here** — unlike `SEASON_LEVEL` /
`IMAGINE_PROFESSION_IDS` above, the `NotifyJoinTeam` message tree's tags
were read directly off protoc-generated `FieldNumber` constants in the
BPSR-ZDPS reference tool's .NET metadata, the same provenance as
`WorldNtf.EnterScene`'s opcode. What is **still unconfirmed** is whether this
traffic actually arrives on the connection this crate adopts at all: no
game session was available while building this (issue #146 step 1), so the
decode is inert in practice until someone runs the Setup procedure above
during an in-party session and confirms `GrpcTeamNtf` Notify fragments show
up (or don't) alongside the main service's. If they do, the fixture-based
unit/integration tests already in `crates/protocol/src/decode.rs` and
`crates/protocol/tests/framing.rs` (synthetic payloads only — see below) are
the regression coverage; if the traffic never appears in a real capture, or
its shape doesn't match the field-tag table, that's the next thing to fix
here.

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
