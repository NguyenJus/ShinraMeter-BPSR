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
   Close the meter normally, then also preserve the final log and numbered `.log.1` through `.log.7` siblings from
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

### Investigation record: #458 total HP (2026-09-19)

The latest retained Frost Ogre session was recorded by v0.3.3.  Offline
replay reconstructs three ordinary recognized-boss lifecycles, each ending
from the actor-state death signal; the corresponding local history entries
also exist.  For the selected boss entity, 592 HP-bearing updates have one
unchanged decoded max of **888,735** and current HP stays in **90–888,735**:
no current value exceeds max and there is no max update or truncation signal.
The three corresponding damage totals are **18,576,525–19,494,759**, so the
pill's decoded total plainly disagrees with the encounter totals.  History
records encounter damage, which can include other targets, not boss HP;
these totals must not be used to infer a scale factor or replacement
denominator.

The retained Basilisk capture (v0.3.2) is internally consistent in the same
way: 1,068 HP-bearing updates have one unchanged decoded max of **9,869,185**
and current HP stays in **680–9,869,185**, with no current-over-max reading.
Its four corresponding damage totals are **17,281,445–70,478,265**, again a
real mismatch rather than a malformed current/max pair.  These observations
establish the symptom, not which HP field should replace the pill.

Both dumps are sanitized (`dump_sanitize: true`).  Sanitization explicitly
retains only its `KEEP_ATTRS` list, which includes current HP (`0x2c2e`) and
max HP (`0x2c38`) but strips `AttrMaxHpTotal` (`0x2c39`, 11321) before the
dump is written.  The source names the latter a rollup and notes that no
tracker currently reads it; that makes it a candidate to observe, not a
supported replacement.  A sanitized dump therefore cannot rule out that or
another scaling/phase field, and cannot justify changing the pill from
decoded `AttrMaxHp`.

The session was still running when inspected, so its shutdown completeness
summary was not available.  A resolving capture must be a short,
unsanitized, locally retained session that is closed normally, with the
visible game current/max HP and meter pill recorded at spawn, after a known
damage interval, and at every scaling or phase transition.  Retain matching
timestamps, difficulty, party size, and all dump-ring chunks privately; add
a synthetic fixture only after those observations establish the expected
total.

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

The retained v0.3.3 sessions on 2026-09-21 and 2026-09-22 did reproduce
**#455** and emitted PR #465's bounded reports: the decoder queue reached its
4,096-event capacity and discarded event batches.  These logs establish that
loss remains possible under busy real sessions.  The accompanying `Pipeline::step`
aggregate stayed sub-microsecond on average with a low-millisecond maximum in
the largest observed batch, but it excludes ticker, command, scheduler, and
lock-wait time, so it cannot identify a consumer bottleneck or attribute loss
to the UI.  A separate long v0.3.3 session closed with the inspect thread
joined and reported zero writer-dropped dump records.  That validates the
inspector-shutdown path, but it is not a complete #425 validation because no
immediate relaunch was retained.

Offline replay of the newest sanitized v0.3.3 dump did not contain a floor-50
run, a #429 phase-group transition, or scene-9300 lifecycle traffic.  It is
therefore not evidence that #456 or #429 is resolved; retain the focused
captures specified above.

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

## Diagnostic builds after the September session investigation

Text logs retain eight 5 MiB chunks (current file plus `.1`–`.7`, about
40 MiB total). Export logs/session bundle includes the complete retained
text ring in chronological order. The packet-dump ring remains independently
bounded at 512 MiB by default; increasing text retention does not increase
packet retention. Busy sessions can still evict early packet data, so export
soon after the symptom.

Startup records link the previous session ID/version and whether its main
function reached completion, including the outcome of explicit worker joins.
A missing completion can mean interruption, forced termination, or a failed
marker write; it is not by itself proof of a crash. Capture shutdown retains
its own outcome messages. UI startup also records creation latency, scale,
and viewport bounds to help distinguish capture stalls from window problems.

The diagnostic changes provide evidence, not fixes for the tracked bugs.
A few sessions must still include the relevant scene/phase/death transitions;
HP interpretation still needs a timestamped comparison with the game's display.
Keep exported artifacts private as above.

On these builds, sanitized dumps retain numeric `AttrMaxHpTotal` (`0x2c39`)
in addition to current HP (`0x2c2e`) and max HP (`0x2c38`). The inspector's
bounded summary reports count, invalid-value count, and min/max for the
candidate; those global extrema are not a boss's HP. To correlate values,
follow `SyncNearEntities` and delta attribute collections by their remapped
UUID, join template `0x0a`, and use scene `0x155`/EnterScene ordering. This
supersedes the older-build limitation above for this specific attribute only;
other unknown fields are still removed. No candidate is used to change meter
HP calculations. Difficulty remains unknown where no validated decoder exists.

For queue loss, look for `pipeline diagnostics` summaries and the existing
capture drop reports together. Counts/rates, sampled queue high-water marks,
step/command/tick/publish timings, and time since the last decoded event
separate several failure paths. Queue depth is only a backlog indicator,
not a measurement of the oldest event's age. Select-loop gaps include idle
waiting and scheduler delay; they do not measure lock contention directly.
Use the surrounding summaries rather than attributing a single long gap to
a particular subsystem.

Encounter start/end/reset/resume/grace lines carry a process-local `fight_id`
and scene/template context. Correlate within a session/PID, not across runs.
History diagnostics identify record outcomes so a missing row can be separated
from an expected retention skip or an enqueue/write failure.
