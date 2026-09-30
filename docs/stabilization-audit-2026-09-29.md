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
  One outer frame now shares a 50 MiB expansion budget across all descendants,
  including failed attempts. Each fragment retains its 10 MiB output limit;
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
  source before writing. File identity checks cover ordinary paths, symlinks,
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
| #274 Windows update | Requires a genuine older-to-newer update with executable locks, elevation, driver handoff, and later cleanup. Unit tests and `--version` do not establish that complete cycle. |
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

## Verification

- `scripts/check.sh`: passed, including formatting, shell packaging/release
  regressions, Windows all-target clippy with warnings denied, full workspace
  tests, and Windows cross-target checking. Workspace tests: **2,196 passed,
  zero failed, one pre-existing ignored test**.
- Application library tests built for `x86_64-pc-windows-gnu` and executed on
  Windows through WSL interop: **1,145 passed, zero failed**.
- All three offline generated-table checks and the upstream drift check passed.
- `cargo deny check`: advisories, bans, licenses, and sources passed after the
  dependency changes.
- Independent high-tier review covered the full diff and cross-module state,
  schema, and update interfaces. Review findings were addressed; no actionable
  findings remain. The reviewer reused author test receipts; the final checks
  above were run separately after integration.
- Optimized Windows cross-build passed; the shared executable inspection passed.
  The updater's actual PE validator accepted the 20,889,088-byte built executable
  and rejected a half-truncated copy.
- Local release `--version` smoke did not complete through WSL: no banner was
  produced before interruption. The release manifest requires elevation before
  `main`; elevation is a suspected limitation, not a verified cause. Native
  library tests above did run successfully. CI retains its elevated Windows
  release-startup smoke check.

These checks do not exercise a live game session or a complete in-place update
transaction. The remaining evidence requirements above are not test passes.
