# Packet inspection: in-game confirmation procedure

Issue #25 built the observation tooling (diagnostic mode — on by default since
issue #87, opt out with `SHINRA_INSPECT=0` — unrecognized service/method id
logging, unknown attr id logging, and a raw frame dump for offline replay).
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
   defaults to `%APPDATA%\ShinraMeter-BPSR\inspect\dump-<pid>.jsonl`).
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
   a count in the replay report below.
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

Run it against a dump:

```
cargo run -p bpsr-protocol --bin inspect-replay -- path/to/dump-<pid>.jsonl
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
