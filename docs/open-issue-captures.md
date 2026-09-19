# Focused sessions for unresolved issues

Use one short session per symptom. These steps collect evidence; they do not
assume the reported behavior is fixed.

## Save a session for later analysis

1. Record the executable version/build, Windows version, timezone, and issue
   number in a local `notes.txt`. Leave diagnostics enabled (`SHINRA_INSPECT=1`).
2. Start the meter before entering the scene. Note wall-clock times for entry,
   pull, each relevant event, and the unexpected behavior. Include expected vs
   actual values, and a screenshot when the discrepancy is visual.
3. Export a session bundle from the header menu immediately after reproducing.
   Close the meter normally, then also preserve the final log and `.log.1` from
   `%APPDATA%\ShinraMeter-BPSR\logs` so shutdown diagnostics are retained.
4. Keep the bundle, notes, screenshots, and any extra captures in a private local
   folder outside the repository. Give the agent that folder's path next session.
   Do not attach raw logs, history databases, names, or captures to public issues.

For **unknown protocol fields**, close the meter first, back up
`%APPDATA%\ShinraMeter-BPSR\settings.json`, and set its existing
`dump_sanitize` property to `false`. Restore it to `true` after the experiment.
Unsanitized dumps are deliberately excluded from session bundles: after closing,
copy that session's `inspect\dump-<session_id>.jsonl` and all numbered siblings
into the private folder yourself. Match the session id to the startup log. Save
them before another launch can age out or evict the ring. Sanitized dumps discard
unknown fields; they cannot disprove the presence of an unmodeled field.

See [packet inspection](packet-inspection.md) for dump/replay details.

## Combat and capture

| Issues | Short reproduction and notes to retain |
| --- | --- |
| #458: total HP | Use an unsanitized session for one Sea-Ringed Reef pull and one Frost Ogre pull. Record the game's visible current/max HP and the meter pill at spawn, after a known damage interval, and across each phase/scaling change. Record difficulty, party size, and exact screenshot times. Do not infer a scale factor from damage alone. |
| #456: floor-50 reset | Enter floor 50, kill the boss, then wait without manually resetting or starting another fight. Record scene/difficulty, death time, reset time, and whether the fight appears in history. Preserve logs from scene entry through the next fight. |
| #429: phase history | On a build containing PR #463, complete Reef or Goblin King. Note each form's death, next form appearance, first hit, final death, and number of history rows. If possible, leave more than two seconds before the next form's first hit. |
| #449: template identity | Run **base-tier** Mistveil Hunting Ground (scene 5901); preserve the `boss target changed` line with `monster_id`. Separate Towering Ruin 1151–1154 runs can confirm their template ids too. |
| #455: decoded queue loss | Reproduce a busy encounter on a build containing [PR #465](https://github.com/NguyenJus/ShinraMeter-BPSR/pull/465). Note time, party size, UI actions, and any freeze. Keep the entire session log, especially queue-drop summaries and shutdown. A protocol dump alone cannot identify consumer delay. |
| #454: repeated TCP holes | Alongside the meter, record a short full-packet TCP trace on the game interface using a local packet-capture tool. Start before the affected pull; stop immediately after a stall. Retain packet timestamps, sequence numbers, payload lengths, and retransmissions. Save the trace privately with the matching log and note tool/interface, capture-drop count, VPN/accelerator state, and stall times. A decoded dump cannot reconstruct missing TCP segments. |
| #425: field validation | On a current build, complete one Chaotic dungeon, close normally, and immediately relaunch. Record any blank meter, stalls, server-change reset, or already-running error. Keep both session ids and logs; old v0.3.0 logs cannot validate newer fixes. |
| #380: proxy ownership | Record accelerator/helper name and version. Reproduce no-adoption with it enabled, then repeat with it disabled. Save both session logs and the helper/game process names and PIDs locally; note whether the helper owns the server TCP socket. Do not change ownership policy based only on a VPN being installed. |

## Protocol discovery

Use unsanitized dumps for these controlled experiments. Do not change reported
damage or add field semantics until the observations establish their meaning.

| Issues | Short reproduction and notes to retain |
| --- | --- |
| #345: damage tag 7 | Hit one target with a known skill for several isolated hits, then repeat under a known mitigation/shield change. Note hit times, visible damage numbers, skill, and target state so `Value` and tag 7 can be compared. |
| #289: attr 0x1bb | Record one full boss pull with timestamped gauge/phase observations, including spawn, gauge changes, and death. Multiple values for the same entity are needed. |
| #288: movement/facing | On one character, stand still, turn in place, move straight without turning, then stop. Note the start/end of each interval. |
| #285: death/revive/buffs | Record one raid with a death, revive, and known buff application/expiry. Note times and who received each effect using local aliases. Existing decoders still need field confirmation; an old issue description is not proof the decoder is absent. |
| #290: list attrs | Defer until a meter feature needs these fields. If investigating, change one equipment/loadout slot at a time while stationary and note each before/after item and timestamp. |

## Other blockers

- **#274, updater:** use the issue's controlled older/newer Windows build setup.
  Record update start/restart times, build versions, drive layout, download hash,
  rename results, UAC count, new process integrity level, and `.old` cleanup after
  the next full close/relaunch. Keep Procmon evidence and both logs locally. This
  requires a real Windows run; cross-compilation is insufficient.
- **#351, release profile:** benchmark the same commit/toolchain on Windows for
  baseline thin LTO and candidate profiles. Retain executable sizes, repeated
  startup timings, and frame-time measurements under the same workload. Linux
  build duration is not a substitute for Windows startup/frame time.
- **#402/#403, UI:** when validating the new history rows and toolbar, include
  window width, display scale, and a screenshot of any remaining overlap. Check
  left-click expansion and right-click loading against two different fights.
- **#165, README:** explicitly requests a human rewrite; leave it for the author.
