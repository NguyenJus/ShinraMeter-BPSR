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
  (`0x2c39`); neither explains encounter damage totals above 17 million.
  Therefore replacing the pill with `0x2c39` would be an unproven semantic
  change and would not resolve #458.
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
  available `0x2c39` evidence rejects it as a simple replacement but does not
  identify the player-visible combat-total quantity.
- **#454:** collect a simultaneous full TCP trace around a hole. It must
  establish whether the missing ranges were absent at capture ingress,
  filtered, or mishandled after parsing before changing reassembly policy.
- The three reference repositories are useful corroboration, not a protocol
  specification; their differing permissiveness and version drift must not
  override a controlled current-client capture.
