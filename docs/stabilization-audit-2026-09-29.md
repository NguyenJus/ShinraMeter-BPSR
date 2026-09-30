# Repository stabilization audit September 2026

This audit starts from `f9aa626` (v0.3.5) and covers transport and decoding,
combat lifecycle and history, application persistence and updates, and release
checks. It preserves this meter's encounter and attribution policies;
reference trackers provide corroboration, not a replacement specification.
Private session artifacts remain outside the repository. This is a bounded
correctness audit, not a guarantee against future game-protocol changes.

## Transport and protocol fixes

- TCP recovery counted healthy idle time against a newly opened sequence gap.
  A regression receives out-of-order segments after 60 seconds of idle, then
  their missing prefix two milliseconds later. Previously this declared loss;
  the gap clock now starts when the gap opens. Persistent-hole recovery,
  backoff, sequence wraparound, and cache bounds remain covered.
- Rejecting an oversized frame discarded a coalesced valid successor. Splitting
  its header after four or five bytes also broke recovery. Regressions cover
  header splits and recovery into the following valid frame.
- Nested compressed siblings could exceed the intended decompression bounds.
  One decoder push now shares a 50 MiB expansion budget across all outer
  frames and descendants, including conservatively counted failed reads. Each
  fragment retains its 10 MiB output limit;
  zstd's history window is capped at 16 MiB. Decoded payloads are released
  after each outer frame. Extreme valid traffic exceeding these limits is
  intentionally refused; these are resource limits, not inferred game semantics.
- Blob fields could consume bytes beyond their declared structure boundary,
  including a child consuming its parent's data. The reader now enforces the
  declared body and END position. Existing padded and unpadded fixtures pass.
- Numeric `ActualValue` (damage tag 7) now survives sanitized dumps. The pinned
  reference declares a signed 64-bit field; its live meaning remains unknown.
  Tests verify retention and identity remapping without changing reported
  damage. Older sanitized dumps discarded this field, and proto3 zero cannot
  distinguish an absent field from an explicitly encoded zero.

## Combat and history fixes

- A next pull, manual reset, shutdown, or scene transition during end grace
  could persist the earlier history snapshot. A synthetic encounter with 275
  damage saved only 200. The meter exposes the completed encounter at its
  actual reset boundary; the pipeline saves final combat statistics while
  preserving the outgoing encounter's identity. It also handles idle end and
  reset occurring in the same damage event.
- A recognized boss killed by the opening hit remained active because fight
  initialization followed death handling. The opening-hit path now applies
  the existing end policy after accumulation, respecting the configured death
  behavior and incomplete-objective/other-boss guards. History's recording
  latch is reset for each new encounter, including immediately ended fights.
- The obsolete ignored shutdown replay now calls the production finalizer and
  checks trailing grace damage and exactly-once persistence; separate abrupt-drop
  coverage remains enabled.
- A slow UI caused a full snapshot queue to rebuild the snapshot unnecessarily.
  The pipeline now reuses the rejected snapshot. A deterministic burst exercises
  20,480 events, 20 players, 64 skills per player, a full 4,096-event ingress
  queue, and a stalled UI consumer. This verifies accounting and progress;
  its producer blocks after the initial burst, so it does not prove capture's
  nonblocking sender cannot overflow under arbitrary load.
- Unknown-schema recovery could overwrite an earlier database backup. It now
  chooses an unused numbered backup. Initial schema creation and its version
  stamp share a transaction.
- History requests after the worker disconnects now return errors instead of
  leaving the UI waiting for replies that will never arrive.
- History sanitization and diagnostic export reject destinations aliasing their
  sources before writing. Bundle preflight checks every planned output,
  including the manifest, against all inputs and live history even when history
  inclusion is disabled. The UI also protects excluded raw captures, the name
  cache, and instance lock files. Preflight runs before the initial log copy
  under the rotation lock. File identity checks cover ordinary paths, symlinks,
  and hard links, with regressions proving source preservation.

## Updater and release fixes

- The updater formerly accepted any body beginning with `MZ`, even two bytes.
  It now verifies GitHub's SHA-256 for the selected asset and checks AMD64
  PE32+ headers and section bounds before staging. Missing or invalid digests
  require a manual download. Hash verification establishes correspondence with
  authenticated GitHub metadata, not an independent publisher signature.
- HTTP bodies have fixed receive chunks, allocation-error handling, a 2 MiB
  metadata cap, a 256 MiB executable cap, and a ten-minute body-loop budget
  alongside existing operation timeouts. An in-flight operation can finish
  after the overall budget until its individual timeout expires.
- Staging flushes before replacement, removes incomplete writes, and restores
  the outgoing executable if replacement process creation fails. A child that
  starts successfully but fails later during initialization remains outside
  this rollback guarantee.
- Release assertions piped inspection tools into `grep -q` under `pipefail`.
  Large output reproduced SIGPIPE/exit 141 despite a valid manifest marker.
  Both workflows now use one script that finishes inspection before searching
  and rejects inspection-tool failures. Tests cover large output, forbidden
  imports, missing manifests/files, and failed tools.
- Windows CI now runs the headless application library tests in addition to
  the release build and `--version` smoke check.

## New session evidence and remaining issues

The newest retained dump rings were analyzed privately by entity, normal player
hit, and death boundary. For an Iron Fang encounter (template 10081, scene 73),
269 HP observations had no upward deltas and summed to 7,326,158 downward HP,
against decoded max HP 7,356,100. Normal player damage to that same target
before actor-state death totaled 45,038,196. Other retained encounters show a
similar discrepancy. This removes unrelated targets and overkill alone as
explanations for #458; it still does not identify the player-visible HP
quantity or a valid universal replacement denominator.

| Issue | Conclusion and required evidence |
| --- | --- |
| #458 boss HP | Stronger target-specific mismatch confirmed above. Keep existing field semantics until a controlled game-visible HP comparison and relevant wire fields establish the correct quantity. |
| #454 TCP holes | The idle-clock defect is fixed, but is not established as the cause of the 13 retained stall recoveries. A simultaneous full TCP trace is still needed to distinguish missing capture ingress, loss, filtering, and reassembly. |
| #455 queue loss | Removed duplicate snapshot work and added a stalled-UI burst regression. Retained queue-full events establish historical loss; instrumented sessions do not isolate its cause. No claim of eliminating arbitrary-load overflow. |
| #456 floor 50 | Older scene-9300 logs show inter-wave idle resets and dungeon completion, but the matching dump is no longer retained. Newest three dump rings do not contain that scene. Related history fixes do not prove this report resolved. |
| #449 template identity | Scene 1154 directly observes damaged Kartgriff template 1152, corroborating that curated tier. Still no scene-5901 evidence; do not select a template from a matching name alone. |
| #380 proxy ownership | No paired helper-enabled/disabled transport and ownership evidence. Preserve the current known-other-process rejection rather than weakening adoption security on repeated signatures. |
| #345 ActualValue | Reference type confirmed; no validated replacement semantics. Future sanitized dumps retain the numeric field, reducing the need for unsanitized collection. |
| #274 Windows update | Native update, instance/driver handoff, rollback failures, lock recovery and later restart passed on Windows 11 build 26200; see the September 30 results and fixture limits below. This does not establish behavior on every installation. |
| #351 release profiles | No comparative Windows startup/frame-time benchmark; retain the existing release profile. |
| #165 README | Explicitly requests a human rewrite; left for the author. |

The existing bounded worker shutdown can still abandon a slow final flush after
its eight-second shared budget, with warning logs. This audit does not replace
that policy or claim durability after forced termination/disk failure.
See [focused capture procedures](open-issue-captures.md) for remaining field work.

## Reference data and provenance

Authenticated GitHub metadata confirmed that this project and all referenced
repositories are public. Their HEADs remain the September 25 audit versions:

- [BPSR-ZDPS cfeb58c](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/tree/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d)
- [bpsr-logs 3b94f4c](https://github.com/winjwinj/bpsr-logs/tree/3b94f4c87e3d9d8b81fe48614425164ad654424f)
- [resonance-logs f4aff36](https://github.com/resonance-logs/resonance-logs/tree/f4aff36e573674e04db1bb09216c603ddf9fb7f6)

The scheduled September 28 name-table job failed because the upstream curated
skill-name projection had 16 additional entries. The targeted refresh adds those
names without replacing existing names or changing boss/scene policy. Source:
[BPSR-ZDPS SkillOverrides.en.json](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPS/Data/SkillOverrides.en.json).
The tag-7 schema comes from [generated Csharp.cs](https://github.com/Blue-Protocol-Source/BPSR-ZDPS/blob/cfeb58c0acc85bc17181b413b9e50b0b26c15c5d/BPSR-ZDPSLib/protos/Csharp.cs#L22946).

## Initial PR verification

- `scripts/check.sh`: passed, including formatting, shell packaging/release
  regressions, Windows all-target clippy with warnings denied, full workspace
  tests, and Windows cross-target checking. After enabling the obsolete skipped
  shutdown regression, the replay suite passed **38 tests, zero ignored**; its
  Windows clippy and formatting checks also passed. Reusing unchanged suite
  results gives **2,197 passing workspace tests, zero failed, zero ignored**.
- Application library tests built for `x86_64-pc-windows-gnu` and executed on
  Windows through WSL interop: **1,145 passed, zero failed**.
- All three offline generated-table checks and the upstream drift check passed.
- `cargo deny check`: advisories, bans, licenses, and sources passed after the
  dependency changes.
- Independent high-tier review covered the full diff and cross-module state,
  schema, and update interfaces. Findings identified in that pass were addressed.
  The reviewer reused author test receipts; the final checks
  above were run separately after integration.
- Optimized Windows cross-build passed; the shared executable inspection passed.
  The updater's actual PE validator accepted the 20,889,088-byte built executable
  and rejected a half-truncated copy.
- Local release `--version` smoke did not complete through WSL: no banner was
  produced before interruption. The release manifest requires elevation before
  `main`; elevation is a suspected limitation, not a verified cause. Native
  library tests above did run successfully. Hosted Windows CI also passed the
  release build, library tests, and elevated release `--version` smoke check.

These initial checks did not exercise a live game session or a complete in-place
update transaction. The September 30 follow-up below records native UI coverage.

## Adversarial PR follow-up

A separate review of PR #481 reproduced additional boundary failures:

- Cross-artifact export aliases could overwrite live history, including through
  `manifest.json`. Complete preflight now rejects them before any bundle write.
  It also includes the sanitizer's destination journal: error cleanup could
  otherwise delete a live source at that path. Direct sanitization protects its
  source against both destination paths before attempting the copy or cleanup.
- History discarded boss identity learned during end grace in the same scene.
  Regressions cover grace expiry, early reset, first scene discovery, and a town
  transition after late boss identity arrives.
- Coalesced compressed outer frames each received a fresh expansion allowance.
  A 2,624-byte synthetic batch expanded 72 MiB. The allowance now spans a whole
  decoder push, with frame alignment retained for recovery on the next push.
- Five immediately invalid compressed siblings exhausted the allowance without
  producing output and suppressed a valid sibling. Failed decompression now
  uses bounded reads and conservatively counts the failed read's capacity,
  including bytes that zstd can produce before returning a checksum error.
  Each attempt costs at least 64 KiB of allowance, capped by the remaining
  fragment allowance, keeping immediately invalid attempts cheap but bounded.
- The stalled-UI regression supplied display UIDs instead of full entity IDs,
  so it never built focused breakdowns. It now uses full player IDs and checks
  all 64 dealt-skill entries per player. Decompression diagnostics also use a
  fragment-neutral label.

Follow-up verification:

- Independent adversarial re-review found no remaining actionable findings in
  the final fixes. The original journal-deletion probes now reject the operation
  and preserve the live source without copying any bundle entries.
- `scripts/check.sh` passed: formatting, both shell regression suites, Windows
  all-target clippy, **2,212 workspace tests (zero failed or ignored)**, and
  Windows cross-target checking.
- Native Windows application library tests executed through WSL interop:
  **1,156 passed, zero failed**, including export with the real instance lock
  held and both journal-source preservation regressions.
- The optimized Windows release build and `scripts/check-windows-exe.sh` passed.
- These headless tests do not replace the real elevated older-to-newer updater
  transaction, driver/instance handoff, rollback, or post-restart cleanup test.
  The elevated UI harness was not running during this follow-up.

## September 30 native UI verification

Native Windows UI testing completed on Windows 11 Pro build 26200 using isolated
installations, settings, history, logs and instance locks. The reviewed candidate
rendered and its menu, update check, history list/details, and export UI worked.
No additional product defects were reproduced.

| Case | Result |
| --- | --- |
| Genuine update | Passed authenticated GitHub download/digest verification, staging, swap and elevated relaunch into published v0.3.5. The predecessor exited and exactly one responsive successor remained. |
| Instance and capture handoff | The successor waited for the previous instance lock, acquired it after shutdown, started WinDivert and adopted the running game's connection. This verifies resumed capture, not combat-statistical correctness. |
| Cleanup and ordinary restart | Staging and backup files disappeared; a subsequent normal restart remained usable with neither file present. |
| Process-creation failure | Deleting the new target after verified swap forced real process creation to fail. The old overlay stayed usable, showed retry feedback, and restored the exact original executable bytes. |
| Restoration failure | A separate lock denied deletion/rename of the backup. The overlay stayed usable; its full error identified the retained backup path, whose bytes matched the original. Releasing the injected lock permitted manual restoration. |
| Validation and file locks | Digest mismatch and invalid PE fixtures left the installation and process intact with retry/error UI. Locking the current executable blocked swap without corrupting it; retry succeeded after releasing the lock. |
| Reviewed-candidate successor | A local-asset fixture installed the original reviewed candidate; its hash matched, it acquired the instance lock, started WinDivert and rendered successfully. |
| Paths | Updates passed with spaces and Unicode in the installation path, including a 240-character executable path. Paths beyond the Windows legacy limit were not tested. |
| History and settings | Four synthetic encounters and their details loaded. All four persisted across normal close/restart without duplication, together with the changed opacity setting. |
| Safe export | Export succeeded under the live instance lock. Sanitized history retained four encounters and 20 player rows; source names were replaced by pseudonyms and SQLite integrity passed. |
| Export aliases | Manifest, settings, name-cache, history, sanitizer-journal and raw-dump aliases were rejected with visible feedback and unchanged protected-source hashes. |
| Unsanitized dump exclusion | A nonempty synthetic raw dump was omitted. Its marker was absent from every exported file, and the manifest explicitly recorded exclusion. |

The genuine network transaction used a disposable build of the reviewed source
reporting v0.3.4, because both the candidate and current published release report
v0.3.5. It installed the actual published asset; the separate local-asset case
covered the reviewed candidate as successor. The published asset SHA-256 was
`8f1d1418c7fb70db868f86b87d2f3b827ca61d5c80e967ec11706828e15f624c`; the original
reviewed candidate was
`0ff2189a4fbbd03d082408ed6fb3e0356f937279370b92f93be9e35023d0e583`.

Disposable test hooks provided a bounded post-swap pause for fault injection,
local bytes with an explicit digest for validation/candidate tests, and a bundle
destination override for pre-existing alias directories. Local-asset cases
bypassed HTTP acquisition and asset-digest provenance; alias cases bypassed only
the native destination chooser. Production validation, swap, relaunch, export
preflight and UI feedback remained in use. The genuine network case left these
hooks disabled. No hooks or fixture executables are included in the PR.

Independent evidence verification found no source-hash changes from the reviewed
candidate and no mismatches in the completed receipts. Logs and screenshots are
retained privately; none are published here. These results do not establish
recovery after a successor is created successfully but later fails initialization,
or resolve unrelated live-combat/protocol issues. No issues were closed.
