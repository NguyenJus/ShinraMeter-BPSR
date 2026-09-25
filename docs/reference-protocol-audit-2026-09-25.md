# Protocol and capture reference audit (2026-09-25)

This audit used the following public repositories at the listed commits:

- [Blue-Protocol-Source/BPSR-ZDPS `cfeb58c`](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/tree/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d)
- [winjwinj/bpsr-logs `3b94f4c`](https://github.com/winjwinj/bpsr-logs/tree/3b94f4c87e3d9d8b81fe48614425164ad654424f)
- [resonance-logs/resonance-logs `f4aff36`](https://github.com/resonance-logs/resonance-logs/tree/f4aff36e573674e04db1bb09216c603ddf9fb7f6)

All three repositories were public when checked with authenticated GitHub
metadata. Local captures and logs were inspected privately; no capture,
player identifier, session identifier, or raw packet bytes are recorded here.

## Findings

- The implemented outer frame format agrees with the references: a
  big-endian length prefix followed by a fragment type, optional zstd payload,
  and nested `FrameDown` stream. This decoder is stricter and bounded where
  the references are permissive; no incompatible framing behavior was found.
- Damage decoding agrees on `SyncDamageInfo`'s damage fields and type routing.
  In particular, using `hp_lessen_value` only to repair a negative damage
  value remains intentional: it is not a replacement for ordinary damage.
- Each reference reads `AttrMaxHp` (`0x2c38`) for enemy HP. BPSR-ZDPS names
  `0x2c39` `AttrMaxHpTotal`, but its own encounter code still reads
  `AttrMaxHp`; its wipe code explicitly leaves a comment that `MaxHpTotal`
  might need investigation. Neither Rust reference models `0x2c39` as an
  enemy-health input.
- The newest sanitized private dump now retains `0x2c39`. Per-monster replay
  shows it tracks `0x2c38` closely, commonly equal for small entities and
  slightly lower for larger ones. For the previously investigated Basilisk
  template, the retained pair is 9,869,185 (`0x2c38`) and 9,869,086
  (`0x2c39`). The older encounter totals exceed 17 million, but may include
  other targets and are not an independent boss-HP measurement. These closely
  tracking fields do not establish a replacement denominator for #458; a
  controlled comparison with the game's visible HP remains necessary.
- Current TCP diagnostics show real persistent sequence holes before the
  stall guard re-anchors. Capture ingress did not report queue drops, but
  that does not prove every network segment reached the application. The
  reassembler preserves contiguous cached runs after a re-anchor and reports
  loss to reset the stateful decoder. No implementation defect can be
  separated from capture/network loss without a matched full TCP trace.

## Audit method

A private one-off replay grouped the retained sanitized `0x2c2e`/`0x2c38`/
`0x2c39` values by monster template. It emitted no entity UUID, player name,
raw attr bytes, or timestamps. The meter's live HP semantics were unchanged.

## Unresolved work

- **#458:** retain a focused controlled Frost Ogre and Sea-Ringed Reef
  session with the game's displayed current/max values and timestamps. The
  available `0x2c39` evidence does not establish it as a valid replacement or
  identify the player-visible combat-total quantity.
- **#454:** collect a simultaneous full TCP trace around a hole. It must
  establish whether the missing ranges were absent at capture ingress,
  filtered, or mishandled after parsing before changing reassembly policy.
- The three reference repositories are useful corroboration, not a protocol
  specification; their differing permissiveness and version drift must not
  override a controlled current-client capture.

## Aggregation and history follow-up

The companion audit checked attacker/target attribution, normal versus miss
handling, crit/lucky hit denominators, shared encounter DPS duration, and
history rehydration. Core attribution and aggregation were internally
consistent. The reference Rust trackers also calculate crit/lucky rates over
hits ([bpsr-logs](https://github.com/winjwinj/bpsr-logs/blob/3b94f4c87e3d9d8b81fe48614425164ad654424f/src-tauri/src/live/commands.rs#L184-L200),
[resonance-logs](https://github.com/resonance-logs/resonance-logs/blob/f4aff36e573674e04db1bb09216c603ddf9fb7f6/src-tauri/src/live/event_manager.rs#L622-L631)).

The audit found that history omitted death time and the
heal/dealt/received/cast/buff breakdowns. Commits `2bae804` and `1f9dea9` identify
these as intentionally deferred persistence work. Merged PR #479 resolves
#477 for v0.3.5: new records preserve those values, while old records remain
readable with unknown death time and empty previously unavailable tabs.

Per-skill Lucky columns differ from the reference UI, but the underlying
counters are accumulated correctly. Their deliberate omission from `SkillRow`
is a feature difference, not evidence of incorrect damage computation; this
audit does not expand the UI merely to match another tracker.

## Previously unidentified attributes

The pinned BPSR-ZDPS generated enum resolves identities that older exploratory
issues treated as guesses. These are reference-derived names, not validation
of the current client's numeric units or update semantics.

| Issue | Attribute | Reference identity |
| --- | --- | --- |
| #288 | `0x32`, `0x33` | `AttrDir`, `AttrTargetDir` |
| #288 | `0x104`–`0x109` | `AttrJumpStep`, `AttrJumpDir`, `AttrVerVelocity`, `AttrHorVelocity`, `AttrJumpType`, `AttrGravity` |
| #288 | `0x1da` | `AttrHateList` (not part of the direction block) |
| #289 | `0x1bb` | `AttrStunned` (not established as an enrage/phase gauge) |
| #290 | `0xc351`, `0xc352` | `AttrFightResourceIds`, `AttrFightResources` (not equipment/loadout IDs) |

Sources: [direction enum](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/EnumEAttrType.cs#L813),
[jump/movement enum](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/EnumEAttrType.cs#L936),
[stunned enum](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/EnumEAttrType.cs#L1019),
[hate-list enum](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/EnumEAttrType.cs#L1038),
[resource enum](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/EnumEAttrType.cs#L2081).
The reference's resource-list parsing is commented out, so it is not evidence
of a working decoder. No new decoder or UI gauge is introduced from these
names alone; any future consumer still needs validated encoding and units.
